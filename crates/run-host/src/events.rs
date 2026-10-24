use fluxaxiom_contract::{
    Artifact, ArtifactCreatedPayload, ArtifactId, AttemptEndedPayload, AttemptId,
    AttemptOrphanedPayload, AttemptStartedPayload, EmptyPayload, Event, EventBody, EventId,
    ModelCallCompletedPayload, RunId, RunTerminalPayload, ToolCallCompletedPayload, SCHEMA_VERSION,
};
use fluxaxiom_engine_api::{BlobRef, EngineObservation};
use rusqlite::{params, Connection, Transaction};
use serde_json::Value;

use crate::blob;
use crate::error::HostError;
use crate::time::now_utc;

/// 在调用方已经打开的事务里追加一条 event。
///
/// **作用**：把 `EventBody` 拆成 `event_type` 与 `payload` 两列，和契约 JSON 的形状一致。
/// **边界**：不负责 `BEGIN` / `COMMIT`，也不分配序号。序号由调用方传入，撞上 `UNIQUE(run_id, sequence)` 时事务失败。
pub(crate) fn record(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    sequence: u64,
    subject_id: impl Into<String>,
    body: EventBody,
) -> Result<(), HostError> {
    let event = Event {
        event_id: EventId::generate(),
        schema_version: SCHEMA_VERSION.to_owned(),
        run_id: run_id.clone(),
        sequence,
        subject_id: subject_id.into(),
        occurred_at: now_utc(),
        body,
    };
    if !event.invariants_hold() {
        return Err(HostError::Invalid("event violates invariants"));
    }
    let value = serde_json::to_value(&event.body).map_err(|_| HostError::Corrupt("event body"))?;
    let event_type = value
        .get("event_type")
        .and_then(Value::as_str)
        .ok_or(HostError::Corrupt("event type"))?;
    let payload = serde_json::to_string(value.get("payload").unwrap_or(&Value::Null))
        .map_err(|_| HostError::Corrupt("event payload"))?;
    let sequence = i64::try_from(sequence).map_err(|_| HostError::Corrupt("sequence"))?;
    transaction.execute(
        "INSERT INTO events (
            event_id, schema_version, run_id, sequence, subject_id, occurred_at, event_type, payload
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.event_id.as_str(),
            event.schema_version,
            event.run_id.as_str(),
            sequence,
            event.subject_id,
            event.occurred_at.as_str(),
            event_type,
            payload,
        ],
    )?;
    Ok(())
}

/// 读取该 Run 已写入的最大序号。
///
/// **作用**：下一条 event 用返回值加 1。
/// **边界**：调用方必须已经处在事务里。写序号用 `IMMEDIATE`，快照只读用 `DEFERRED`。没有 event 时返回 0。
pub(crate) fn max_sequence(
    transaction: &Transaction<'_>,
    run_id: &RunId,
) -> Result<u64, HostError> {
    let max: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE run_id = ?1",
        params![run_id.as_str()],
        |row| row.get(0),
    )?;
    u64::try_from(max).map_err(|_| HostError::Corrupt("sequence"))
}

/// 按序号补读已提交的 event。
///
/// **作用**：拼回契约 `Event`，供 `events_after` 和快照使用。
/// **边界**：只返回 `sequence > after` 的行。`schema_version` 不是当前草案时视为库损坏。
pub(crate) fn load_after(
    connection: &Connection,
    run_id: &RunId,
    after: u64,
) -> Result<Vec<Event>, HostError> {
    let after = i64::try_from(after).map_err(|_| HostError::Corrupt("sequence"))?;
    let mut statement = connection.prepare(
        "SELECT event_id, schema_version, run_id, sequence, subject_id, occurred_at, event_type, payload
         FROM events WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence",
    )?;
    let rows = statement.query_map(params![run_id.as_str(), after], map_event_row)?;
    rows.collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(event_from_raw)
        .collect()
}

/// 在同一事务里写入 Artifact 索引。
///
/// **作用**：保存类型、摘要和 blob 引用，供 `artifact.created` 与快照使用。
/// **边界**：正文不进这张表。调用方须先把正文写成 blob，本函数不检查文件是否存在。
pub(crate) fn insert_artifact(
    transaction: &Transaction<'_>,
    artifact: &Artifact,
) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO artifacts (
            artifact_id, run_id, artifact_type, storage_ref, summary, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            artifact.artifact_id.as_str(),
            artifact.run_id.as_str(),
            artifact.artifact_type,
            artifact.storage_ref,
            artifact.summary,
            artifact.created_at.as_str(),
        ],
    )?;
    Ok(())
}

/// 列出该 Run 的 Artifact 主键。
///
/// **作用**：按创建时间和 `artifact_id` 稳定排序，填进 `RunSnapshot`。
/// **边界**：只读索引表，不打开 blob 文件。
pub(crate) fn artifact_ids(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Vec<ArtifactId>, HostError> {
    let mut statement = connection.prepare(
        "SELECT artifact_id FROM artifacts WHERE run_id = ?1 ORDER BY created_at, artifact_id",
    )?;
    let rows = statement.query_map(params![run_id.as_str()], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|value| ArtifactId::parse(value).map_err(|_| HostError::Corrupt("artifact_id")))
        .collect()
}

/// 列出该 Run 的全部 Artifact 行。
///
/// **作用**：CLI `artifacts` 用；与 `artifact_ids` 排序一致。快照只要 id，走窄查询。
/// **边界**：只读索引表；`summary` 是短摘要，正文在 `storage_ref` 指向的 blob。
pub(crate) fn list_artifacts(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Vec<Artifact>, HostError> {
    let mut statement = connection.prepare(
        "SELECT artifact_id, run_id, artifact_type, storage_ref, summary, created_at
         FROM artifacts WHERE run_id = ?1 ORDER BY created_at, artifact_id",
    )?;
    let rows = statement.query_map(params![run_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut artifacts = Vec::new();
    for row in rows {
        let (artifact_id, run_id_value, artifact_type, storage_ref, summary, created_at) = row?;
        artifacts.push(Artifact {
            artifact_id: ArtifactId::parse(artifact_id)
                .map_err(|_| HostError::Corrupt("artifact_id"))?,
            run_id: RunId::parse(run_id_value).map_err(|_| HostError::Corrupt("run_id"))?,
            artifact_type,
            storage_ref,
            summary,
            created_at: fluxaxiom_contract::Timestamp::parse(created_at)
                .map_err(|_| HostError::Corrupt("artifact created_at"))?,
        });
    }
    Ok(artifacts)
}

/// 把观测变成契约 payload。
///
/// **作用**：确认字段和 blob 都齐，再交给 `record` 写入。本函数不写库。
/// **边界**：空字符串、空引用、引用格式不对、blob 不存在，都是 `Invalid`。
/// 工具 `byte_count` 必须等于 `result_ref` 内容的长度，也就是返回给模型的字节数。
pub(crate) fn observation_body(
    root: &std::path::Path,
    observation: &EngineObservation,
) -> Result<EventBody, HostError> {
    match observation {
        EngineObservation::ModelCall {
            model_alias,
            usage,
            finish_reason,
            latency_ms,
            request_ref,
            response_ref,
        } => {
            require_text(model_alias, "model_alias")?;
            require_text(finish_reason, "finish_reason")?;
            require_stored(root, request_ref)?;
            require_stored(root, response_ref)?;
            Ok(EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                model_alias: model_alias.clone(),
                usage: usage.clone(),
                finish_reason: finish_reason.clone(),
                latency_ms: *latency_ms,
                request_ref: request_ref.as_str().to_owned(),
                response_ref: response_ref.as_str().to_owned(),
            }))
        }
        EngineObservation::ToolCall {
            tool_name,
            normalized_path_or_query,
            result_ref,
            byte_count,
            truncated,
            duration_ms,
        } => {
            require_text(tool_name, "tool_name")?;
            require_text(normalized_path_or_query, "normalized_path_or_query")?;
            let stored = blob::stored_len(root, result_ref)?;
            if stored != *byte_count {
                return Err(HostError::Invalid(
                    "byte_count must equal the blob returned to the model",
                ));
            }
            Ok(EventBody::ToolCallCompleted(ToolCallCompletedPayload {
                tool_name: tool_name.clone(),
                normalized_path_or_query: normalized_path_or_query.clone(),
                result_ref: result_ref.as_str().to_owned(),
                byte_count: *byte_count,
                truncated: *truncated,
                duration_ms: *duration_ms,
            }))
        }
    }
}

/// `run.accepted`：Run 刚创建、尚未 drive。
///
/// **作用**：供 `create_run` 与状态变更同事务写入。
/// **边界**：空 payload；不携带密钥或任务原文。
pub(crate) fn accepted() -> EventBody {
    EventBody::RunAccepted(EmptyPayload {})
}

/// `run.started`：已进入 `Running`。
///
/// **作用**：供 `prepare_drive` 与 Attempt 启动同事务写入。
/// **边界**：空 payload。
pub(crate) fn started() -> EventBody {
    EventBody::RunStarted(EmptyPayload {})
}

/// `run.cancelling`：取消意图已落库。
///
/// **作用**：供 `request_cancel` 在 `Running → Cancelling` 时写入（A3b）。
/// **边界**：空 payload；不表示已经终态。
pub(crate) fn cancelling() -> EventBody {
    EventBody::RunCancelling(EmptyPayload {})
}

/// `attempt.orphaned`：租约过期收尸。
///
/// **作用**：供 `orphan_drive` 与 `run.failed(host_restart)` 同事务写入。
/// **参数**：`attempt_id` 是被标成 `Orphaned` 的那一行。
/// **边界**：不携带租约时间戳；时间在 Attempt 行上。
pub(crate) fn attempt_orphaned(attempt_id: AttemptId) -> EventBody {
    EventBody::AttemptOrphaned(AttemptOrphanedPayload { attempt_id })
}

/// `attempt.started`：Attempt 进入 `Running`。
///
/// **作用**：供 `prepare_drive` 写入 owner 与首次租约截止时间。
/// **参数**：`owner` 是本进程标识；`lease_expires_at` 为 `now + TTL`。
/// **边界**：续期只改 Attempt 行，不再发本事件。
pub(crate) fn attempt_started(
    attempt_id: AttemptId,
    owner: String,
    lease_expires_at: fluxaxiom_contract::Timestamp,
) -> EventBody {
    EventBody::AttemptStarted(AttemptStartedPayload {
        attempt_id,
        owner,
        lease_expires_at,
    })
}

/// `attempt.ended`：Attempt 离开 `Running` 进入正常终态。
///
/// **作用**：供 `finish_drive` 与 Run 终态同事务写入。
/// **参数**：`outcome` 是 Attempt 契约 outcome，不是 Engine 原始枚举。
/// **边界**：孤儿收尸走 [`attempt_orphaned`]，不走本函数。
pub(crate) fn attempt_ended(
    attempt_id: AttemptId,
    outcome: fluxaxiom_contract::AttemptOutcome,
) -> EventBody {
    EventBody::AttemptEnded(AttemptEndedPayload {
        attempt_id,
        outcome,
    })
}

/// 按 `TerminalReason` 选出 `run.completed` / `run.cancelled` / `run.failed`。
///
/// **作用**：让 Host 只传原因，不必在每个调用点重复 match。
/// **参数**：`reason` 写入 payload 的 `terminal_reason`。
/// **边界**：`HostRestart` / `EngineError` / `ProviderError` / `BudgetExhausted` 都映射到 `run.failed`。
pub(crate) fn terminal(reason: fluxaxiom_contract::TerminalReason) -> EventBody {
    let payload = RunTerminalPayload {
        terminal_reason: reason,
    };
    match reason {
        fluxaxiom_contract::TerminalReason::Completed => EventBody::RunCompleted(payload),
        fluxaxiom_contract::TerminalReason::Cancelled => EventBody::RunCancelled(payload),
        fluxaxiom_contract::TerminalReason::BudgetExhausted
        | fluxaxiom_contract::TerminalReason::HostRestart
        | fluxaxiom_contract::TerminalReason::EngineError
        | fluxaxiom_contract::TerminalReason::ProviderError => EventBody::RunFailed(payload),
    }
}

/// `artifact.created`：索引行已写入。
///
/// **作用**：供 `finish_drive` 在落 Artifact 后追加。
/// **参数**：`artifact` 的摘要字段进入 payload；正文仍只在 blob。
/// **边界**：不打开 `storage_ref` 指向的文件。
pub(crate) fn artifact_created(artifact: &Artifact) -> EventBody {
    EventBody::ArtifactCreated(ArtifactCreatedPayload {
        artifact_id: artifact.artifact_id.clone(),
        artifact_type: artifact.artifact_type.clone(),
        storage_ref: artifact.storage_ref.clone(),
        summary: artifact.summary.clone(),
    })
}

/// 观测字段非空校验。
///
/// **作用**：把空串收成带字段名的 `Invalid`，供 `observation_body` 使用。
/// **参数**：`label` 是错误消息里的字段名。
/// **边界**：只查空串，不查空白字符。
fn require_text(value: &str, label: &'static str) -> Result<(), HostError> {
    if value.is_empty() {
        Err(HostError::Invalid(label))
    } else {
        Ok(())
    }
}

/// 确认 blob 引用在磁盘上存在。
///
/// **作用**：观测里的 `request_ref` / `response_ref` 必须先 `put_blob`。
/// **边界**：不存在或路径非法时透传 `blob::stored_len` 的错误。
fn require_stored(root: &std::path::Path, blob_ref: &BlobRef) -> Result<(), HostError> {
    blob::stored_len(root, blob_ref).map(|_| ())
}

/// events 表一行的原始列，尚未校验契约。
struct EventRow {
    event_id: String,
    schema_version: String,
    run_id: String,
    sequence: i64,
    subject_id: String,
    occurred_at: String,
    event_type: String,
    payload: String,
}

/// 把 SQLite 行收成 [`EventRow`]。
///
/// **作用**：供 `load_after` 的 `query_map` 使用。
/// **边界**：不做契约校验；非法内容留给 [`event_from_raw`]。
fn map_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        event_id: row.get(0)?,
        schema_version: row.get(1)?,
        run_id: row.get(2)?,
        sequence: row.get(3)?,
        subject_id: row.get(4)?,
        occurred_at: row.get(5)?,
        event_type: row.get(6)?,
        payload: row.get(7)?,
    })
}

/// 把原始行收成契约 `Event`。
///
/// **作用**：校验 schema、id、时间戳与 `EventBody` 反序列化，再跑 `invariants_hold`。
/// **边界**：任一字段不合法返回 `Corrupt`，不尝试修复。
fn event_from_raw(row: EventRow) -> Result<Event, HostError> {
    if row.schema_version != SCHEMA_VERSION {
        return Err(HostError::Corrupt("schema_version"));
    }
    let payload: Value =
        serde_json::from_str(&row.payload).map_err(|_| HostError::Corrupt("event payload"))?;
    let tagged = serde_json::json!({
        "event_type": row.event_type,
        "payload": payload,
    });
    let body: EventBody =
        serde_json::from_value(tagged).map_err(|_| HostError::Corrupt("event body"))?;
    let event = Event {
        event_id: EventId::parse(row.event_id).map_err(|_| HostError::Corrupt("event_id"))?,
        schema_version: row.schema_version,
        run_id: RunId::parse(row.run_id).map_err(|_| HostError::Corrupt("run_id"))?,
        sequence: u64::try_from(row.sequence).map_err(|_| HostError::Corrupt("sequence"))?,
        subject_id: row.subject_id,
        occurred_at: fluxaxiom_contract::Timestamp::parse(row.occurred_at)
            .map_err(|_| HostError::Corrupt("occurred_at"))?,
        body,
    };
    if event.invariants_hold() {
        Ok(event)
    } else {
        Err(HostError::Corrupt("event"))
    }
}
