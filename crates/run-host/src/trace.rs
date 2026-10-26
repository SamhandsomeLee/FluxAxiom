//! 从已提交事件组装 Run 时间线（A4a / §4.11）。
//!
//! **作用**：把 `model.call_completed` / `tool.call_completed` 收成「轮次 → 工具」结构，供 `flux trace` 打印或 JSON 导出。
//! **边界**：只理解事件与 `*_ref`，不解析 Provider JSON。坏 blob 由调用方读时标 `unavailable`，本模块不读文件。

use std::collections::HashMap;

use fluxaxiom_contract::{
    Attempt, AttemptOutcome, Budget, Event, EventBody, ModelUsage, RunSnapshot, RunStatus,
    TerminalReason, Timestamp, TokenCount,
};
use serde::{Deserialize, Serialize};

/// 一次 Run 的只读时间线快照。
///
/// **作用**：CLI `flux trace` 的结构化结果；`--json` 直接序列化本类型。
/// **边界**：字段名固定，供 A4b 样本记录扩展。不含 prompt / `reasoning_content` / 报告正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTrace {
    pub run_id: String,
    pub status: RunStatus,
    pub terminal_reason: Option<TerminalReason>,
    pub workspace_ref: String,
    pub engine_id: String,
    pub engine_version: String,
    pub attempt_id: String,
    pub attempt_outcome: Option<AttemptOutcome>,
    pub budget: Budget,
    /// 轮次列表。`index == 0` 仅含首个模型调用之前的工具；模型轮从 1 起。
    pub rounds: Vec<TraceRound>,
    pub artifacts: Vec<TraceArtifact>,
    pub totals: TraceTotals,
}

/// 一个分组轮次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRound {
    pub index: u32,
    /// `#0` 没有模型调用；`#1…` 必有。
    pub model: Option<TraceModelCall>,
    pub tools: Vec<TraceToolCall>,
}

/// 一轮模型调用的事件摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceModelCall {
    pub seq: u64,
    pub occurred_at: String,
    pub model_alias: String,
    pub finish_reason: String,
    pub latency_ms: u64,
    pub usage: ModelUsage,
    pub request_ref: String,
    pub response_ref: String,
}

/// 一轮内的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceToolCall {
    /// 旧事件缺失时为 None，仍可展示；校验不得推断调用 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub seq: u64,
    pub tool_name: String,
    pub normalized_path_or_query: String,
    pub result_ref: String,
    pub byte_count: u64,
    pub truncated: bool,
    pub duration_ms: u64,
}

/// 报告 Artifact 摘要行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceArtifact {
    pub artifact_id: String,
    pub artifact_type: String,
    pub storage_ref: String,
    pub bytes: Option<u64>,
}

/// 合计行：token 任一轮为 `unknown` 则合计为 `unknown`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceTotals {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
    pub prompt_tokens: TokenCount,
    pub completion_tokens: TokenCount,
    pub prompt_cache_hit_tokens: TokenCount,
    pub prompt_cache_miss_tokens: TokenCount,
    /// `run.started` 到终态事件的秒差；缺任一侧为 `None`。
    pub wall_seconds: Option<u64>,
}

/// 从快照、Attempt 与事件组装 [`RunTrace`]。
///
/// **作用**：`RunHost::trace` 读库后调用；也可供单测直接喂事件。
/// **参数**：`events` 须按 `sequence` 升序；`artifact_bytes` 以 `artifact_id` 为键，只含读得到 blob 的 Artifact，缺项显示为 `unavailable`。
/// **边界**：不读 blob 正文。不要求 Run 已终态。
pub fn build_trace(
    snapshot: &RunSnapshot,
    attempt: &Attempt,
    events: &[Event],
    artifact_bytes: &HashMap<String, u64>,
) -> RunTrace {
    let mut rounds: Vec<TraceRound> = Vec::new();
    let mut pending_tools: Vec<TraceToolCall> = Vec::new();
    let mut artifacts: Vec<TraceArtifact> = Vec::new();
    let mut started_at: Option<&Timestamp> = None;
    let mut ended_at: Option<&Timestamp> = None;

    for event in events {
        match &event.body {
            EventBody::RunStarted(_) => {
                started_at = Some(&event.occurred_at);
            }
            EventBody::RunCompleted(_) | EventBody::RunFailed(_) | EventBody::RunCancelled(_) => {
                ended_at = Some(&event.occurred_at);
            }
            EventBody::ModelCallCompleted(payload) => {
                flush_pending_tools(&mut rounds, &mut pending_tools);
                let index = rounds.iter().filter(|round| round.model.is_some()).count() as u32 + 1;
                rounds.push(TraceRound {
                    index,
                    model: Some(TraceModelCall {
                        seq: event.sequence,
                        occurred_at: event.occurred_at.as_str().to_owned(),
                        model_alias: payload.model_alias.clone(),
                        finish_reason: payload.finish_reason.clone(),
                        latency_ms: payload.latency_ms,
                        usage: payload.usage.clone(),
                        request_ref: payload.request_ref.clone(),
                        response_ref: payload.response_ref.clone(),
                    }),
                    tools: Vec::new(),
                });
            }
            EventBody::ToolCallCompleted(payload) => {
                pending_tools.push(TraceToolCall {
                    tool_call_id: payload.tool_call_id.clone(),
                    seq: event.sequence,
                    tool_name: payload.tool_name.clone(),
                    normalized_path_or_query: payload.normalized_path_or_query.clone(),
                    result_ref: payload.result_ref.clone(),
                    byte_count: payload.byte_count,
                    truncated: payload.truncated,
                    duration_ms: payload.duration_ms,
                });
            }
            EventBody::ArtifactCreated(payload) => {
                artifacts.push(TraceArtifact {
                    artifact_id: payload.artifact_id.as_str().to_owned(),
                    artifact_type: payload.artifact_type.clone(),
                    storage_ref: payload.storage_ref.clone(),
                    bytes: artifact_bytes.get(payload.artifact_id.as_str()).copied(),
                });
            }
            _ => {}
        }
    }
    flush_pending_tools(&mut rounds, &mut pending_tools);

    let model_calls = rounds.iter().filter(|r| r.model.is_some()).count() as u32;
    let tool_calls = rounds.iter().map(|r| r.tools.len() as u32).sum();
    let mut totals = TraceTotals {
        model_calls,
        tool_calls,
        max_model_calls: snapshot.budget.max_model_calls,
        max_tool_calls: snapshot.budget.max_tool_calls,
        prompt_tokens: TokenCount::Known(0),
        completion_tokens: TokenCount::Known(0),
        prompt_cache_hit_tokens: TokenCount::Known(0),
        prompt_cache_miss_tokens: TokenCount::Known(0),
        wall_seconds: wall_seconds(started_at, ended_at),
    };
    for round in &rounds {
        if let Some(model) = &round.model {
            totals.prompt_tokens = add_tokens(totals.prompt_tokens, model.usage.prompt_tokens);
            totals.completion_tokens =
                add_tokens(totals.completion_tokens, model.usage.completion_tokens);
            totals.prompt_cache_hit_tokens = add_tokens(
                totals.prompt_cache_hit_tokens,
                model.usage.prompt_cache_hit_tokens,
            );
            totals.prompt_cache_miss_tokens = add_tokens(
                totals.prompt_cache_miss_tokens,
                model.usage.prompt_cache_miss_tokens,
            );
        }
    }

    RunTrace {
        run_id: snapshot.run_id.as_str().to_owned(),
        status: snapshot.status,
        terminal_reason: snapshot.terminal_reason,
        workspace_ref: snapshot.workspace_ref.clone(),
        engine_id: attempt.engine_id.clone(),
        engine_version: attempt.engine_version.clone(),
        attempt_id: attempt.attempt_id.as_str().to_owned(),
        attempt_outcome: attempt.outcome,
        budget: snapshot.budget.clone(),
        rounds,
        artifacts,
        totals,
    }
}

/// 把 [`RunTrace`] 收成 §4.11 默认人类可读文本。
///
/// **作用**：`flux trace` 非 `--json` 路径；单测可直接断言全文。
/// **边界**：不读 blob；不输出 `reasoning_content`。
pub fn format_trace(trace: &RunTrace) -> String {
    let mut out = String::new();
    let terminal = match &trace.terminal_reason {
        Some(reason) => format!("{reason:?}"),
        None => "None".to_owned(),
    };
    out.push_str(&format!(
        "run_id={} status={:?} terminal_reason={}\n",
        trace.run_id, trace.status, terminal
    ));
    out.push_str(&format!(
        "workspace={}\n",
        display_path(&trace.workspace_ref)
    ));
    let outcome = match trace.attempt_outcome {
        Some(value) => format!("{value:?}").to_ascii_lowercase(),
        None => "none".to_owned(),
    };
    out.push_str(&format!(
        "engine={} {} attempt={} outcome={}\n",
        trace.engine_id, trace.engine_version, trace.attempt_id, outcome
    ));
    for round in &trace.rounds {
        if let Some(model) = &round.model {
            out.push_str(&format!(
                "#{} model={} seq={} finish={} latency={}ms prompt={} hit={} miss={} completion={}\n",
                round.index,
                model.model_alias,
                model.seq,
                model.finish_reason,
                model.latency_ms,
                format_tokens(model.usage.prompt_tokens),
                format_tokens(model.usage.prompt_cache_hit_tokens),
                format_tokens(model.usage.prompt_cache_miss_tokens),
                format_tokens(model.usage.completion_tokens),
            ));
        } else {
            out.push_str(&format!(
                "#{} (tools before first model call)\n",
                round.index
            ));
        }
        for tool in &round.tools {
            out.push_str(&format!(
                "     tool {} {:?} seq={} bytes={} truncated={} {}ms\n",
                tool.tool_name,
                tool.normalized_path_or_query,
                tool.seq,
                tool.byte_count,
                tool.truncated,
                tool.duration_ms
            ));
        }
    }
    for artifact in &trace.artifacts {
        let bytes = match artifact.bytes {
            Some(n) => n.to_string(),
            None => "unavailable".to_owned(),
        };
        out.push_str(&format!(
            "artifact {} {} storage_ref={} bytes={}\n",
            artifact.artifact_type, artifact.artifact_id, artifact.storage_ref, bytes
        ));
    }
    let wall = match trace.totals.wall_seconds {
        Some(s) => format!("{s}s"),
        None => "unknown".to_owned(),
    };
    out.push_str(&format!(
        "totals model_calls={}/{} tool_calls={}/{} prompt={} hit={} miss={} completion={} wall={}\n",
        trace.totals.model_calls,
        trace.totals.max_model_calls,
        trace.totals.tool_calls,
        trace.totals.max_tool_calls,
        format_tokens(trace.totals.prompt_tokens),
        format_tokens(trace.totals.prompt_cache_hit_tokens),
        format_tokens(trace.totals.prompt_cache_miss_tokens),
        format_tokens(trace.totals.completion_tokens),
        wall,
    ));
    out
}

/// 把尚未挂上的工具调用归入当前轮；若还没有模型轮，先开 `#0`。
fn flush_pending_tools(rounds: &mut Vec<TraceRound>, pending: &mut Vec<TraceToolCall>) {
    if pending.is_empty() {
        return;
    }
    if let Some(last) = rounds.last_mut() {
        last.tools.append(pending);
    } else {
        rounds.push(TraceRound {
            index: 0,
            model: None,
            tools: std::mem::take(pending),
        });
    }
}

/// 去掉 Windows 规范化路径的 `\\?\` 前缀，只用于显示。
///
/// **作用**：`canonicalize` 在 Windows 上返回 verbatim 路径，直接打印很难读。
/// **边界**：`\\?\UNC\…` 保持原样（去掉前缀会变成另一条路径）。`--json` 不经过这里，保留库里存的原值。
fn display_path(path: &str) -> &str {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") => rest,
        _ => path,
    }
}

fn format_tokens(count: TokenCount) -> String {
    match count {
        TokenCount::Known(n) => n.to_string(),
        TokenCount::Unknown => "unknown".to_owned(),
    }
}

fn add_tokens(left: TokenCount, right: TokenCount) -> TokenCount {
    match (left, right) {
        (TokenCount::Known(a), TokenCount::Known(b)) => TokenCount::Known(a.saturating_add(b)),
        _ => TokenCount::Unknown,
    }
}

fn wall_seconds(started: Option<&Timestamp>, ended: Option<&Timestamp>) -> Option<u64> {
    let started = started?;
    let ended = ended?;
    parse_unix_seconds(started.as_str())
        .and_then(|a| parse_unix_seconds(ended.as_str()).map(|b| b.saturating_sub(a)))
}

/// 解析契约 UTC 时间戳为 unix 秒。精度到秒；带小数时截断。
fn parse_unix_seconds(raw: &str) -> Option<u64> {
    let trimmed = raw.strip_suffix('Z')?;
    let head = trimmed.split('.').next()?;
    let (date, time) = head.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u8 = date_parts.next()?.parse().ok()?;
    let day: u8 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: u8 = time_parts.next()?.parse().ok()?;
    let minute: u8 = time_parts.next()?.parse().ok()?;
    let second: u8 = time_parts.next()?.parse().ok()?;
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let time = time::Time::from_hms(hour, minute, second).ok()?;
    let datetime = time::PrimitiveDateTime::new(date, time).assume_utc();
    u64::try_from(datetime.unix_timestamp()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{
        AttemptId, AttemptStatus, EmptyPayload, EventId, ModelCallCompletedPayload, RunId,
        ToolCallCompletedPayload, SCHEMA_VERSION,
    };

    fn ts(raw: &str) -> Timestamp {
        Timestamp::parse(raw).unwrap()
    }

    fn event(seq: u64, at: &str, body: EventBody) -> Event {
        Event {
            event_id: EventId::generate(),
            schema_version: SCHEMA_VERSION.to_owned(),
            run_id: RunId::generate(),
            sequence: seq,
            subject_id: "sub".to_owned(),
            occurred_at: ts(at),
            body,
        }
    }

    fn usage(
        prompt: TokenCount,
        hit: TokenCount,
        miss: TokenCount,
        completion: TokenCount,
    ) -> ModelUsage {
        ModelUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
        }
    }

    fn snapshot_and_attempt() -> (RunSnapshot, Attempt) {
        let run_id = RunId::generate();
        let attempt_id = AttemptId::generate();
        let snapshot = RunSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            run_id: run_id.clone(),
            session_id: None,
            status: RunStatus::Completed,
            terminal_reason: Some(TerminalReason::Completed),
            last_sequence: 10,
            input_ref: "sha256:aa".to_owned(),
            workspace_ref: "/tmp/ws".to_owned(),
            budget: Budget {
                max_model_calls: 8,
                max_tool_calls: 32,
            },
            artifact_ids: vec![],
            updated_at: ts("2026-10-26T00:00:10Z"),
        };
        let attempt = Attempt {
            attempt_id,
            run_id,
            engine_id: "engine-a".to_owned(),
            engine_version: "0.1.0".to_owned(),
            owner: "pid-1".to_owned(),
            lease_expires_at: ts("2026-10-26T00:01:00Z"),
            status: AttemptStatus::Ended,
            outcome: Some(AttemptOutcome::Completed),
            error_reason: None,
        };
        (snapshot, attempt)
    }

    /// 工具出现在首个模型调用之前，归入 `#0`。
    #[test]
    fn tools_before_first_model_go_to_round_zero() {
        let (snapshot, attempt) = snapshot_and_attempt();
        let events = vec![
            event(
                1,
                "2026-10-26T00:00:00Z",
                EventBody::RunStarted(EmptyPayload {}),
            ),
            event(
                2,
                "2026-10-26T00:00:01Z",
                EventBody::ToolCallCompleted(ToolCallCompletedPayload {
                    tool_call_id: None,
                    tool_name: "list_files".to_owned(),
                    normalized_path_or_query: ".".to_owned(),
                    result_ref: "sha256:t1".to_owned(),
                    byte_count: 10,
                    truncated: false,
                    duration_ms: 1,
                }),
            ),
            event(
                3,
                "2026-10-26T00:00:02Z",
                EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                    model_alias: "deepseek-flash".to_owned(),
                    usage: usage(
                        TokenCount::Known(5),
                        TokenCount::Known(0),
                        TokenCount::Known(5),
                        TokenCount::Known(1),
                    ),
                    finish_reason: "stop".to_owned(),
                    latency_ms: 100,
                    request_ref: "sha256:r1".to_owned(),
                    response_ref: "sha256:s1".to_owned(),
                }),
            ),
            event(
                4,
                "2026-10-26T00:00:03Z",
                EventBody::RunCompleted(fluxaxiom_contract::RunTerminalPayload {
                    terminal_reason: TerminalReason::Completed,
                }),
            ),
        ];
        let trace = build_trace(&snapshot, &attempt, &events, &HashMap::new());
        assert_eq!(trace.rounds.len(), 2);
        assert_eq!(trace.rounds[0].index, 0);
        assert!(trace.rounds[0].model.is_none());
        assert_eq!(trace.rounds[0].tools.len(), 1);
        assert_eq!(trace.rounds[1].index, 1);
        assert!(trace.rounds[1].model.is_some());
        assert_eq!(trace.totals.wall_seconds, Some(3));
    }

    /// 任一轮 token 为 unknown，合计也是 unknown。
    #[test]
    fn unknown_tokens_propagate_to_totals() {
        let (snapshot, attempt) = snapshot_and_attempt();
        let events = vec![event(
            1,
            "2026-10-26T00:00:00Z",
            EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                model_alias: "m".to_owned(),
                usage: usage(
                    TokenCount::Known(10),
                    TokenCount::Unknown,
                    TokenCount::Known(10),
                    TokenCount::Known(2),
                ),
                finish_reason: "stop".to_owned(),
                latency_ms: 1,
                request_ref: "sha256:a".to_owned(),
                response_ref: "sha256:b".to_owned(),
            }),
        )];
        let trace = build_trace(&snapshot, &attempt, &events, &HashMap::new());
        assert_eq!(trace.totals.prompt_tokens, TokenCount::Known(10));
        assert_eq!(trace.totals.prompt_cache_hit_tokens, TokenCount::Unknown);
        let text = format_trace(&trace);
        assert!(text.contains("hit=unknown"));
        assert!(text.contains("totals") && text.contains("hit=unknown"));
    }

    /// `--json` 往返保持同一值。
    #[test]
    fn json_round_trip() {
        let (snapshot, attempt) = snapshot_and_attempt();
        let events = vec![event(
            1,
            "2026-10-26T00:00:00Z",
            EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                model_alias: "m".to_owned(),
                usage: usage(
                    TokenCount::Known(1),
                    TokenCount::Known(0),
                    TokenCount::Known(1),
                    TokenCount::Known(1),
                ),
                finish_reason: "stop".to_owned(),
                latency_ms: 2,
                request_ref: "sha256:a".to_owned(),
                response_ref: "sha256:b".to_owned(),
            }),
        )];
        let trace = build_trace(&snapshot, &attempt, &events, &HashMap::new());
        let value = serde_json::to_value(&trace).unwrap();
        let back: RunTrace = serde_json::from_value(value).unwrap();
        assert_eq!(back, trace);
    }

    /// 同轮重复工具不去重。
    #[test]
    fn duplicate_tools_in_one_round_are_kept() {
        let (snapshot, attempt) = snapshot_and_attempt();
        let tool = |seq, path: &str| {
            event(
                seq,
                "2026-10-26T00:00:01Z",
                EventBody::ToolCallCompleted(ToolCallCompletedPayload {
                    tool_call_id: None,
                    tool_name: "list_files".to_owned(),
                    normalized_path_or_query: path.to_owned(),
                    result_ref: format!("sha256:t{seq}"),
                    byte_count: 1,
                    truncated: false,
                    duration_ms: 1,
                }),
            )
        };
        let events = vec![
            event(
                1,
                "2026-10-26T00:00:00Z",
                EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                    model_alias: "m".to_owned(),
                    usage: usage(
                        TokenCount::Known(1),
                        TokenCount::Known(0),
                        TokenCount::Known(1),
                        TokenCount::Known(1),
                    ),
                    finish_reason: "tool_calls".to_owned(),
                    latency_ms: 1,
                    request_ref: "sha256:a".to_owned(),
                    response_ref: "sha256:b".to_owned(),
                }),
            ),
            tool(2, "."),
            tool(3, "."),
        ];
        let trace = build_trace(&snapshot, &attempt, &events, &HashMap::new());
        assert_eq!(trace.rounds.len(), 1);
        assert_eq!(trace.rounds[0].tools.len(), 2);
    }

    /// 取消停在工具批中途：该轮只有已完成的工具，没有后续轮次，状态取自快照。
    #[test]
    fn run_cut_mid_batch_keeps_completed_tools_only() {
        let (mut snapshot, attempt) = snapshot_and_attempt();
        snapshot.status = RunStatus::Cancelled;
        snapshot.terminal_reason = Some(TerminalReason::Cancelled);
        let events = vec![
            event(
                1,
                "2026-10-26T00:00:00Z",
                EventBody::RunStarted(EmptyPayload {}),
            ),
            event(
                2,
                "2026-10-26T00:00:01Z",
                EventBody::ModelCallCompleted(ModelCallCompletedPayload {
                    model_alias: "m".to_owned(),
                    usage: usage(
                        TokenCount::Known(1),
                        TokenCount::Known(0),
                        TokenCount::Known(1),
                        TokenCount::Known(1),
                    ),
                    finish_reason: "tool_calls".to_owned(),
                    latency_ms: 1,
                    request_ref: "sha256:a".to_owned(),
                    response_ref: "sha256:b".to_owned(),
                }),
            ),
            event(
                3,
                "2026-10-26T00:00:02Z",
                EventBody::ToolCallCompleted(ToolCallCompletedPayload {
                    tool_call_id: None,
                    tool_name: "read_file".to_owned(),
                    normalized_path_or_query: "a.rs".to_owned(),
                    result_ref: "sha256:t".to_owned(),
                    byte_count: 3,
                    truncated: false,
                    duration_ms: 1,
                }),
            ),
            event(
                4,
                "2026-10-26T00:00:05Z",
                EventBody::RunCancelled(fluxaxiom_contract::RunTerminalPayload {
                    terminal_reason: TerminalReason::Cancelled,
                }),
            ),
        ];
        let trace = build_trace(&snapshot, &attempt, &events, &HashMap::new());
        assert_eq!(trace.status, RunStatus::Cancelled);
        assert_eq!(trace.rounds.len(), 1);
        assert_eq!(trace.rounds[0].tools.len(), 1);
        assert_eq!(trace.totals.model_calls, 1);
        assert_eq!(trace.totals.wall_seconds, Some(5));
    }

    /// Artifact 字节数按 `artifact_id` 对应；缺项显示 `unavailable`，不按下标错位。
    #[test]
    fn artifact_bytes_are_matched_by_id() {
        let (snapshot, attempt) = snapshot_and_attempt();
        let first = fluxaxiom_contract::ArtifactId::generate();
        let second = fluxaxiom_contract::ArtifactId::generate();
        let created = |seq, id: &fluxaxiom_contract::ArtifactId| {
            event(
                seq,
                "2026-10-26T00:00:01Z",
                EventBody::ArtifactCreated(fluxaxiom_contract::ArtifactCreatedPayload {
                    artifact_id: id.clone(),
                    artifact_type: "report".to_owned(),
                    storage_ref: format!("sha256:{seq}"),
                    summary: "s".to_owned(),
                }),
            )
        };
        let events = vec![created(1, &first), created(2, &second)];
        let mut bytes = HashMap::new();
        bytes.insert(second.as_str().to_owned(), 42);
        let trace = build_trace(&snapshot, &attempt, &events, &bytes);
        assert_eq!(trace.artifacts[0].bytes, None);
        assert_eq!(trace.artifacts[1].bytes, Some(42));
        assert!(format_trace(&trace).contains("bytes=unavailable"));
    }

    /// 显示时去掉 `\\?\` 前缀；UNC 形式与普通路径不动。
    #[test]
    fn display_path_strips_verbatim_prefix_only() {
        assert_eq!(display_path(r"\\?\E:\work\ws"), r"E:\work\ws");
        assert_eq!(display_path(r"\\?\UNC\host\share"), r"\\?\UNC\host\share");
        assert_eq!(display_path("/tmp/ws"), "/tmp/ws");
    }
}
