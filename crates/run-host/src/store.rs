use std::path::Path;
use std::time::Duration;

use fluxaxiom_contract::{
    Attempt, AttemptId, AttemptOutcome, AttemptStatus, Budget, Run, RunId, RunStatus, Session,
    SessionId, TerminalReason, Timestamp,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::HostError;
use crate::time::now_utc;

/// 打开文件、启用 WAL，并让写冲突等待 5 秒而不是立刻 `SQLITE_BUSY`。
pub(crate) fn connect(path: &Path) -> Result<Connection, HostError> {
    let connection = Connection::open(path)?;
    configure(&connection)?;
    Ok(connection)
}

pub(crate) fn configure(connection: &Connection) -> Result<(), HostError> {
    // journal_mode 会返回当前模式，不能用只执行语句的 pragma_update。
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(HostError::Corrupt("journal_mode is not WAL"));
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(Duration::from_millis(5_000))?;
    Ok(())
}

/// 打开一条会写 event 的事务。
///
/// **作用**：`BEGIN IMMEDIATE`，先拿 reserved lock，再让调用方读状态和 `MAX(sequence)`。
/// **边界**：只包 SQLite 里的行。blob 文件的写入发生在事务外面。状态迁移是否允许必须在这条事务里判断。
pub(crate) fn immediate(connection: &mut Connection) -> Result<Transaction<'_>, HostError> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// 打开一条一致的只读事务。
///
/// **作用**：让快照里的状态和 `MAX(sequence)` 来自同一时刻。
/// **边界**：`DEFERRED`，不拿 reserved lock，不用于分配序号或改状态。
pub(crate) fn deferred(connection: &mut Connection) -> Result<Transaction<'_>, HostError> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Deferred)?)
}

/// 按版本顺序应用迁移。已记录的版本不会重跑，历史行不原地改写。
pub(crate) fn migrate(connection: &mut Connection) -> Result<(), HostError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;
    let current: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    const MIGRATIONS: &[&str] = &[
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_events.sql"),
    ];
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).expect("migration count fits i64");
        if version <= current {
            continue;
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_utc().as_str()],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

pub(crate) fn insert_session(
    transaction: &Transaction<'_>,
    session: &Session,
) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO sessions (session_id, created_at) VALUES (?1, ?2)",
        params![session.session_id.as_str(), session.created_at.as_str()],
    )?;
    Ok(())
}

pub(crate) fn insert_created(
    transaction: &Transaction<'_>,
    run: &Run,
    attempt: &Attempt,
    request_id: &str,
) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO runs (
            run_id, request_id, session_id, status, terminal_reason,
            input_ref, workspace_ref, max_model_calls, max_tool_calls,
            created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.run_id.as_str(),
            request_id,
            run.session_id.as_ref().map(SessionId::as_str),
            encode_run_status(run.status),
            run.terminal_reason.map(encode_terminal_reason),
            run.input_ref,
            run.workspace_ref,
            run.budget.max_model_calls,
            run.budget.max_tool_calls,
            run.created_at.as_str(),
            run.updated_at.as_str(),
        ],
    )?;
    insert_attempt(transaction, attempt)?;
    Ok(())
}

fn insert_attempt(transaction: &Transaction<'_>, attempt: &Attempt) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO attempts (
            attempt_id, run_id, engine_id, engine_version, owner,
            lease_expires_at, status, outcome, error_reason
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            attempt.attempt_id.as_str(),
            attempt.run_id.as_str(),
            attempt.engine_id,
            attempt.engine_version,
            attempt.owner,
            attempt.lease_expires_at.as_str(),
            encode_attempt_status(attempt.status),
            attempt.outcome.map(encode_attempt_outcome),
            attempt.error_reason,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_session(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Session, HostError> {
    let raw = connection
        .query_row(
            "SELECT session_id, created_at FROM sessions WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    session_from_raw(raw)
}

pub(crate) fn load_run(connection: &Connection, run_id: &RunId) -> Result<Run, HostError> {
    let raw = connection
        .query_row(
            "SELECT run_id, session_id, status, terminal_reason, input_ref, workspace_ref,
                    max_model_calls, max_tool_calls, created_at, updated_at
             FROM runs WHERE run_id = ?1",
            params![run_id.as_str()],
            map_run_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    run_from_raw(raw)
}

pub(crate) fn load_attempt(
    connection: &Connection,
    attempt_id: &AttemptId,
) -> Result<Attempt, HostError> {
    let raw = connection
        .query_row(
            "SELECT attempt_id, run_id, engine_id, engine_version, owner, lease_expires_at,
                    status, outcome, error_reason
             FROM attempts WHERE attempt_id = ?1",
            params![attempt_id.as_str()],
            map_attempt_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    attempt_from_raw(raw)
}

pub(crate) fn load_created_by_request_id(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<(Run, Attempt)>, HostError> {
    let Some(raw) = connection
        .query_row(
            "SELECT run_id, session_id, status, terminal_reason, input_ref, workspace_ref,
                    max_model_calls, max_tool_calls, created_at, updated_at
             FROM runs WHERE request_id = ?1",
            params![request_id],
            map_run_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    let run = run_from_raw(raw)?;
    let attempt = latest_attempt(connection, &run.run_id)?;
    Ok(Some((run, attempt)))
}

pub(crate) fn latest_attempt(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Attempt, HostError> {
    let raw = connection
        .query_row(
            "SELECT attempt_id, run_id, engine_id, engine_version, owner, lease_expires_at,
                    status, outcome, error_reason
             FROM attempts WHERE run_id = ?1
             ORDER BY attempt_id DESC LIMIT 1",
            params![run_id.as_str()],
            map_attempt_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    attempt_from_raw(raw)
}

/// 在同一事务里更新 Run 与 Attempt 行。
///
/// **作用**：状态变更、续期、取消、收尸共用；Attempt 的 `lease_expires_at` 一并写入。
/// **参数**：`run` / `attempt` 是调用方已改好的内存快照，本函数不做转移校验。
/// **边界**：任一行影响行数不是 1 返回 `NotFound`。不写 durable event。
pub(crate) fn update_run_and_attempt(
    transaction: &Transaction<'_>,
    run: &Run,
    attempt: &Attempt,
) -> Result<(), HostError> {
    let changed = transaction.execute(
        "UPDATE runs
         SET status = ?1, terminal_reason = ?2, updated_at = ?3
         WHERE run_id = ?4",
        params![
            encode_run_status(run.status),
            run.terminal_reason.map(encode_terminal_reason),
            run.updated_at.as_str(),
            run.run_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(HostError::NotFound);
    }
    let changed = transaction.execute(
        "UPDATE attempts
         SET status = ?1, outcome = ?2, error_reason = ?3, lease_expires_at = ?4
         WHERE attempt_id = ?5",
        params![
            encode_attempt_status(attempt.status),
            attempt.outcome.map(encode_attempt_outcome),
            attempt.error_reason,
            attempt.lease_expires_at.as_str(),
            attempt.attempt_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(HostError::NotFound);
    }
    Ok(())
}

/// 列出可能需要收孤儿的 Attempt：所属 Run 为 `Running` / `Cancelling`，Attempt 仍为 `Running`。
///
/// **作用**：启动扫描的候选集；调用方再按 `lease_expires_at` 过滤。
/// **边界**：不在 SQL 里比时间，避免时区/格式分歧；调用方用 [`crate::time::lease_is_expired`]。
pub(crate) fn list_active_drive_attempts(
    connection: &Connection,
) -> Result<Vec<(Run, Attempt)>, HostError> {
    let mut statement = connection.prepare(
        "SELECT r.run_id, r.session_id, r.status, r.terminal_reason, r.input_ref, r.workspace_ref,
                r.max_model_calls, r.max_tool_calls, r.created_at, r.updated_at,
                a.attempt_id, a.run_id, a.engine_id, a.engine_version, a.owner, a.lease_expires_at,
                a.status, a.outcome, a.error_reason
         FROM runs r
         INNER JOIN attempts a ON a.run_id = r.run_id
         WHERE r.status IN ('Running', 'Cancelling')
           AND a.status = 'Running'
         ORDER BY r.run_id, a.attempt_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            RunRow {
                run_id: row.get(0)?,
                session_id: row.get(1)?,
                status: row.get(2)?,
                terminal_reason: row.get(3)?,
                input_ref: row.get(4)?,
                workspace_ref: row.get(5)?,
                max_model_calls: row.get(6)?,
                max_tool_calls: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            },
            AttemptRow {
                attempt_id: row.get(10)?,
                run_id: row.get(11)?,
                engine_id: row.get(12)?,
                engine_version: row.get(13)?,
                owner: row.get(14)?,
                lease_expires_at: row.get(15)?,
                status: row.get(16)?,
                outcome: row.get(17)?,
                error_reason: row.get(18)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (run_raw, attempt_raw) = row?;
        out.push((run_from_raw(run_raw)?, attempt_from_raw(attempt_raw)?));
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn journal_mode(connection: &Connection) -> Result<String, HostError> {
    Ok(connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
}

#[cfg(test)]
pub(crate) fn busy_timeout_ms(connection: &Connection) -> Result<i64, HostError> {
    Ok(connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?)
}

struct RunRow {
    run_id: String,
    session_id: Option<String>,
    status: String,
    terminal_reason: Option<String>,
    input_ref: String,
    workspace_ref: String,
    max_model_calls: i64,
    max_tool_calls: i64,
    created_at: String,
    updated_at: String,
}

struct AttemptRow {
    attempt_id: String,
    run_id: String,
    engine_id: String,
    engine_version: String,
    owner: String,
    lease_expires_at: String,
    status: String,
    outcome: Option<String>,
    error_reason: Option<String>,
}

fn map_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: row.get(0)?,
        session_id: row.get(1)?,
        status: row.get(2)?,
        terminal_reason: row.get(3)?,
        input_ref: row.get(4)?,
        workspace_ref: row.get(5)?,
        max_model_calls: row.get(6)?,
        max_tool_calls: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn map_attempt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRow> {
    Ok(AttemptRow {
        attempt_id: row.get(0)?,
        run_id: row.get(1)?,
        engine_id: row.get(2)?,
        engine_version: row.get(3)?,
        owner: row.get(4)?,
        lease_expires_at: row.get(5)?,
        status: row.get(6)?,
        outcome: row.get(7)?,
        error_reason: row.get(8)?,
    })
}

fn session_from_raw((session_id, created_at): (String, String)) -> Result<Session, HostError> {
    Ok(Session {
        session_id: SessionId::parse(session_id).map_err(|_| HostError::Corrupt("session_id"))?,
        created_at: Timestamp::parse(created_at).map_err(|_| HostError::Corrupt("created_at"))?,
    })
}

fn run_from_raw(row: RunRow) -> Result<Run, HostError> {
    Ok(Run {
        run_id: RunId::parse(row.run_id).map_err(|_| HostError::Corrupt("run_id"))?,
        session_id: match row.session_id {
            Some(value) => {
                Some(SessionId::parse(value).map_err(|_| HostError::Corrupt("session_id"))?)
            }
            None => None,
        },
        status: decode_run_status(&row.status)?,
        terminal_reason: match row.terminal_reason {
            Some(value) => Some(decode_terminal_reason(&value)?),
            None => None,
        },
        input_ref: row.input_ref,
        workspace_ref: row.workspace_ref,
        budget: Budget {
            max_model_calls: parse_u32(row.max_model_calls)?,
            max_tool_calls: parse_u32(row.max_tool_calls)?,
        },
        created_at: Timestamp::parse(row.created_at)
            .map_err(|_| HostError::Corrupt("created_at"))?,
        updated_at: Timestamp::parse(row.updated_at)
            .map_err(|_| HostError::Corrupt("updated_at"))?,
    })
}

fn attempt_from_raw(row: AttemptRow) -> Result<Attempt, HostError> {
    Ok(Attempt {
        attempt_id: AttemptId::parse(row.attempt_id)
            .map_err(|_| HostError::Corrupt("attempt_id"))?,
        run_id: RunId::parse(row.run_id).map_err(|_| HostError::Corrupt("run_id"))?,
        engine_id: row.engine_id,
        engine_version: row.engine_version,
        owner: row.owner,
        lease_expires_at: Timestamp::parse(row.lease_expires_at)
            .map_err(|_| HostError::Corrupt("lease_expires_at"))?,
        status: decode_attempt_status(&row.status)?,
        outcome: match row.outcome {
            Some(value) => Some(decode_attempt_outcome(&value)?),
            None => None,
        },
        error_reason: row.error_reason,
    })
}

fn parse_u32(value: i64) -> Result<u32, HostError> {
    u32::try_from(value).map_err(|_| HostError::Corrupt("budget"))
}

fn encode_run_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Accepted => "Accepted",
        RunStatus::Running => "Running",
        RunStatus::Cancelling => "Cancelling",
        RunStatus::Completed => "Completed",
        RunStatus::Failed => "Failed",
        RunStatus::Cancelled => "Cancelled",
    }
}

fn decode_run_status(value: &str) -> Result<RunStatus, HostError> {
    match value {
        "Accepted" => Ok(RunStatus::Accepted),
        "Running" => Ok(RunStatus::Running),
        "Cancelling" => Ok(RunStatus::Cancelling),
        "Completed" => Ok(RunStatus::Completed),
        "Failed" => Ok(RunStatus::Failed),
        "Cancelled" => Ok(RunStatus::Cancelled),
        _ => Err(HostError::Corrupt("run status")),
    }
}

fn encode_terminal_reason(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::Completed => "completed",
        TerminalReason::Cancelled => "cancelled",
        TerminalReason::BudgetExhausted => "budget_exhausted",
        TerminalReason::HostRestart => "host_restart",
        TerminalReason::EngineError => "engine_error",
        TerminalReason::ProviderError => "provider_error",
    }
}

fn decode_terminal_reason(value: &str) -> Result<TerminalReason, HostError> {
    match value {
        "completed" => Ok(TerminalReason::Completed),
        "cancelled" => Ok(TerminalReason::Cancelled),
        "budget_exhausted" => Ok(TerminalReason::BudgetExhausted),
        "host_restart" => Ok(TerminalReason::HostRestart),
        "engine_error" => Ok(TerminalReason::EngineError),
        "provider_error" => Ok(TerminalReason::ProviderError),
        _ => Err(HostError::Corrupt("terminal reason")),
    }
}

fn encode_attempt_status(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Starting => "Starting",
        AttemptStatus::Running => "Running",
        AttemptStatus::Ended => "Ended",
        AttemptStatus::Orphaned => "Orphaned",
    }
}

fn decode_attempt_status(value: &str) -> Result<AttemptStatus, HostError> {
    match value {
        "Starting" => Ok(AttemptStatus::Starting),
        "Running" => Ok(AttemptStatus::Running),
        "Ended" => Ok(AttemptStatus::Ended),
        "Orphaned" => Ok(AttemptStatus::Orphaned),
        _ => Err(HostError::Corrupt("attempt status")),
    }
}

fn encode_attempt_outcome(outcome: AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Completed => "completed",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Cancelled => "cancelled",
    }
}

fn decode_attempt_outcome(value: &str) -> Result<AttemptOutcome, HostError> {
    match value {
        "completed" => Ok(AttemptOutcome::Completed),
        "failed" => Ok(AttemptOutcome::Failed),
        "cancelled" => Ok(AttemptOutcome::Cancelled),
        _ => Err(HostError::Corrupt("attempt outcome")),
    }
}
