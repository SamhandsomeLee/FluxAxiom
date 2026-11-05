//! L1 作废遮蔽：覆盖判断、`mask_superseded` 与按 edit 重放。
//!
//! **作用**：比较 `read_file` 区间，把被后读盖住的旧正文换成存根，并按记录重放。
//! **边界**：不由 `prepare` 调用。不打开 blob。
//!

use crate::transcript::{ToolResult, View};
use serde_json::{json, Value};

use crate::record::{ContextRecord, ContextSpec, ReplayError};
use crate::strategy::ContextStrategy;


/// 一次 `read_file` 实际读到的范围。
///
/// **作用**：`later_covers` 用它比较两条结果。
/// **边界**：`end` 为 `None` 表示直到文件末尾。为 `Some` 时是不含该行的下一行行号。
struct ReadSpan {
    /// 渲染文本里 `path:` 后面的路径。空串表示工具报告的路径就是空的。
    path: String,
    /// 起始行，从 1 起。
    start: u32,
    /// 不含在内的下一行。`None` 表示这次读到了文件末尾。
    end: Option<u32>,
}


impl ReadSpan {
    /// 判断这次读取是否包含另一次读取的全部行。
    ///
    /// **作用**：`later_covers` 在路径相同之后调用。
    /// **参数**：`earlier` 是更早的那次读取。起点必须不小于这次的起点。
    /// **边界**：这次有终点而另一次直到文件末尾时，不包含。
    fn contains(&self, earlier: &ReadSpan) -> bool {
        if self.start > earlier.start {
            return false;
        }
        match (self.end, earlier.end) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(later_end), Some(earlier_end)) => later_end >= earlier_end,
        }
    }
}

/// 判断后一次 `read_file` 是否盖住先一次。
///
/// **作用**：以后的 L1 用它挑选可以换成存根的旧结果。
/// **参数**：`later` 是后一次结果。`earlier` 是先一次结果。两条的 `body` 都是工具渲染文本。
/// **边界**：工具名不是 `read_file`、路径不同，或渲染文本缺路径、起点、截断标记时返回 `false`。不比较 `arguments`。
fn later_covers(later: &ToolResult, earlier: &ToolResult) -> bool {
    let Some(later_span) = read_span(later) else {
        return false;
    };
    let Some(earlier_span) = read_span(earlier) else {
        return false;
    };
    later_span.path == earlier_span.path && later_span.contains(&earlier_span)
}

/// 从渲染文本读出路径和行区间。
///
/// **作用**：`later_covers` 对两条结果各调用一次。
/// **参数**：`tool` 的 `name` 必须是 `read_file`。`body` 要含 `path:`、`start_line:` 和一行截断标记。
/// **边界**：缺任何一行、起点不是从 1 起的整数，或 `next_start_line` 不大于起点时返回 `None`。
fn read_span(tool: &ToolResult) -> Option<ReadSpan> {
    if tool.name != "read_file" {
        return None;
    }
    let mut path = None;
    let mut start = None;
    let mut end = None;
    let mut saw_truncated = false;
    for line in tool.body.lines() {
        if let Some(value) = line.strip_prefix("path: ") {
            path = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("start_line: ") {
            start = value.parse::<u32>().ok().filter(|line_no| *line_no >= 1);
        } else if line == "truncated: false" {
            saw_truncated = true;
            end = None;
        } else if let Some(value) = line.strip_prefix("truncated: true; next_start_line: ") {
            let next = value.parse::<u32>().ok()?;
            let start_line = start.filter(|line_no| next > *line_no)?;
            saw_truncated = true;
            end = Some(next);
            let _ = start_line;
        }
    }
    if !saw_truncated {
        return None;
    }
    Some(ReadSpan {
        path: path?,
        start: start?,
        end,
    })
}

/// 写入记录的 L1 策略标识。
pub const L1_STRATEGY: &str = "l1@1";


/// 作废遮蔽策略。
///
/// **作用**：实现 [`ContextStrategy`]。`apply` 遮掉被后读盖住的旧正文，`replay` 按记录写回。
/// **边界**：没有字段。不读档案，不打开 blob。`prepare` 不调用它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupersedeRead;

impl ContextStrategy for SupersedeRead {
    /// 返回 `l1@1`。
    ///
    /// **作用**：记录和重放用它认出作废遮蔽。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L1_STRATEGY
    }
    /// 把被后读盖住的旧工具正文换成存根，并追加 edit。
    ///
    /// **作用**：策略测试直接调用。
    /// **参数**：`view` 只改保护窗口之外命中的 `body`。`spec` 不读。`record` 只在 `edits` 末尾追加，不改策略名。
    /// **边界**：没有命中时不追加。不删除 blob。
    fn apply(&self, view: &mut View, _spec: &ContextSpec, record: &mut ContextRecord) {
        mask_superseded(view, &mut record.edits);
    }
    /// 按记录把事实视图里的正文写回存根。
    ///
    /// **作用**：`ContextManager::replay` 在策略名是 `l1@1` 时调用。
    /// **参数**：`view` 从事实抄出。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l1@1` 时返回 [`ReplayError::UnknownStrategy`]，不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        replay_superseded_edits(view, &record.edits)
    }
}

/// `prepare` 在留下 L1 时写入的策略标识。
pub const L0L1_STRATEGY: &str = "l0l1@1";

/// 先重放 L0 的 edit，再重放 L1 的 edit。
///
/// **作用**：`ContextManager::replay` 在策略名是 `l0l1@1` 时调用。
/// **边界**：`prepare` 不调用 [`Self::apply`]。不打开 blob。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L0ThenL1;

impl ContextStrategy for L0ThenL1 {
    /// 返回 `l0l1@1`。
    ///
    /// **作用**：记录和重放用它认出「L0 之后又留下了 L1」。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L0L1_STRATEGY
    }

    /// 不在这里跑软触发。
    ///
    /// **作用**：trait 要求有 `apply`。软触发和腾出量在 `prepare` 里。
    /// **参数**：`view`、`spec`、`record` 都不读。空视图允许。
    /// **边界**：不改视图，不追加 edit。
    fn apply(&self, _view: &mut View, _spec: &ContextSpec, _record: &mut ContextRecord) {}

    /// 按 `op` 分成两段重放。
    ///
    /// **作用**：同一条记录里可以同时有 `unchanged` 和 `superseded`。
    /// **参数**：`view` 从事实抄出。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l0l1@1`，或出现第三种 `op`，都不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        let mut unchanged = Vec::new();
        let mut superseded = Vec::new();
        for edit in &record.edits {
            let Some(op) = edit.get("op").and_then(|value| value.as_str()) else {
                return Err(ReplayError::InvalidEdit);
            };
            match op {
                "unchanged" => unchanged.push(edit.clone()),
                "superseded" => superseded.push(edit.clone()),
                _ => return Err(ReplayError::InvalidEdit),
            }
        }
        crate::forward_dedup::replay_unchanged_edits(view, &unchanged)?;
        replay_superseded_edits(view, &superseded)
    }
}

/// 保护窗口：最近多少个模型轮里的工具结果不被 L1 遮掉。
const PROTECT_ROUNDS: usize = 2;

/// 把被后读盖住的旧 `read_file` 正文换成存根。
///
/// **作用**：L1 的 apply 测试直接调用它。
/// **参数**：`view` 只改被遮那条的 `body`。`edits` 只追加命中的 edit。
/// **边界**：不读 `spec`，不打开 blob。没有命中时不改。
fn mask_superseded(view: &mut View, edits: &mut Vec<Value>) {
    let round_count = view.rounds.len();
    let protect_from = round_count.saturating_sub(PROTECT_ROUNDS);
    for earlier_round in 0..round_count {
        if earlier_round >= protect_from {
            continue;
        }
        let tool_count = view.rounds[earlier_round].tools.len();
        for earlier_call in 0..tool_count {
            let earlier = view.rounds[earlier_round].tools[earlier_call].clone();
            if earlier.name != "read_file" || earlier.body.starts_with("[superseded:") {
                continue;
            }
            let Some((by_round, by_call)) =
                latest_covering(&view.rounds, earlier_round, earlier_call, &earlier)
            else {
                continue;
            };
            let Some(stub) = superseded_stub(&earlier, by_round, by_call) else {
                continue;
            };
            view.rounds[earlier_round].tools[earlier_call].body = stub;
            edits.push(json!({
                "op": "superseded",
                "round": earlier_round + 1,
                "call": earlier_call + 1,
                "superseded_by_round": by_round + 1,
                "superseded_by_call": by_call + 1,
                "result_ref": earlier.result_ref,
            }));
        }
    }
}

/// 在更晚的结果里找最后一次仍盖住 `earlier` 的位置。
fn latest_covering(
    rounds: &[crate::transcript::Round],
    earlier_round: usize,
    earlier_call: usize,
    earlier: &ToolResult,
) -> Option<(usize, usize)> {
    let mut found = None;
    for later_round in earlier_round..rounds.len() {
        let start_call = if later_round == earlier_round {
            earlier_call + 1
        } else {
            0
        };
        for later_call in start_call..rounds[later_round].tools.len() {
            let later = &rounds[later_round].tools[later_call];
            if later_covers(later, earlier) {
                found = Some((later_round, later_call));
            }
        }
    }
    found
}

/// 渲染 L1 存根一行。
fn superseded_stub(earlier: &ToolResult, by_round: usize, by_call: usize) -> Option<String> {
    let span = read_span(earlier)?;
    let range = match span.end {
        Some(next) if next > span.start => format!("L{}-{}", span.start, next - 1),
        Some(_) => return None,
        None => format!("L{}-EOF", span.start),
    };
    Some(format!(
        "[superseded: read_file \"{}\" {}, {} B, {}; see round #{} call {}]",
        span.path,
        range,
        earlier.byte_count,
        earlier.result_ref,
        by_round + 1,
        by_call + 1
    ))
}

/// 按 L1 edit 把事实视图里的正文写回存根。
///
/// **作用**：重放测试直接调用。以后的策略 `replay` 再转调它。
/// **参数**：`view` 从事实抄出，成功时只改 edit 点到的 `body`。`edits` 只处理 `op: superseded`。
/// **边界**：先检查全部 edit，任一不合法返回 [`ReplayError::InvalidEdit`] 且不改 `view`。不重新计算覆盖。
pub(crate) fn replay_superseded_edits(view: &mut View, edits: &[Value]) -> Result<(), ReplayError> {
    let mut changes = Vec::new();
    for edit in edits {
        let Some(object) = edit.as_object() else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(op) = object.get("op").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        if op != "superseded" {
            return Err(ReplayError::InvalidEdit);
        }
        let round = one_based(object, "round")?;
        let call = one_based(object, "call")?;
        let by_round = one_based(object, "superseded_by_round")?;
        let by_call = one_based(object, "superseded_by_call")?;
        let Some(result_ref) = object.get("result_ref").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(tool) = tool_at(view, round, call) else {
            return Err(ReplayError::InvalidEdit);
        };
        if tool.result_ref != result_ref {
            return Err(ReplayError::InvalidEdit);
        }
        if tool_at(view, by_round, by_call).is_none() {
            return Err(ReplayError::InvalidEdit);
        }
        if (by_round, by_call) <= (round, call) {
            return Err(ReplayError::InvalidEdit);
        }
        let stub = superseded_stub(tool, by_round, by_call).ok_or(ReplayError::InvalidEdit)?;
        changes.push((round, call, stub));
    }
    for (round, call, body) in changes {
        view.rounds[round].tools[call].body = body;
    }
    Ok(())
}


/// 读取 edit 里从 1 起的序号，并转换成切片下标。
///
/// **作用**：`replay_superseded_edits` 读轮次与调用序号。
/// **参数**：`object` 是一条 edit。`key` 是要读的字段名。值必须是不小于 1 的整数。
/// **边界**：缺字段、负数、小数或 0 都返回 [`ReplayError::InvalidEdit`]。
fn one_based(object: &serde_json::Map<String, Value>, key: &str) -> Result<usize, ReplayError> {
    let Some(number) = object.get(key).and_then(Value::as_u64) else {
        return Err(ReplayError::InvalidEdit);
    };
    let Some(index) = usize::try_from(number)
        .ok()
        .and_then(|value| value.checked_sub(1))
    else {
        return Err(ReplayError::InvalidEdit);
    };
    Ok(index)
}

/// 按从 0 起的下标取出一条工具结果。
///
/// **作用**：`replay_superseded_edits` 确认序号落在视图里。
/// **参数**：`view` 只读。`round` 是轮次下标，`call` 是该轮工具下标，都从 0 起。
/// **边界**：越界时返回 `None`。
fn tool_at(view: &View, round: usize, call: usize) -> Option<&ToolResult> {
    view.rounds
        .get(round)
        .and_then(|item| item.tools.get(call))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{ModelUsage, TokenCount};

    use crate::forward_dedup::L0_STRATEGY;
    use crate::record::{Estimate, EstimateSource, ReplayError};
    use crate::transcript::{AssistantTurn, Round};

    /// 拼一条 `read_file` 的渲染正文。
    ///
    /// **作用**：`read_span` 和 `mask_superseded` 测试共用。
    /// **参数**：`path` 写入 `path:` 行。`start` 是起始行。`truncated` 为真时必须有 `next`。
    /// **边界**：中间正文用 `content` 占位，不参与覆盖判断。
    fn read_body(path: &str, start: u32, truncated: bool, next: Option<u32>, content: &str) -> String {
        let mut text = format!(
            "tool: read_file\npath: {path}\nstart_line: {start}\n\n{content}\n"
        );
        if truncated {
            text.push_str(&format!(
                "truncated: true; next_start_line: {}\n",
                next.expect("truncated read needs next_start_line")
            ));
        } else {
            text.push_str("truncated: false\n");
        }
        text
    }

    /// 造一条带渲染正文的工具结果。
    ///
    /// **作用**：覆盖与遮蔽测试共用。
    /// **参数**：`byte_count` 写入存根。`result_ref` 写入 edit。
    fn read_tool(
        path: &str,
        start: u32,
        truncated: bool,
        next: Option<u32>,
        content: &str,
        byte_count: u64,
        result_ref: &str,
    ) -> ToolResult {
        ToolResult {
            call_id: "call".to_owned(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"ignored"}"#.to_owned(),
            result_ref: result_ref.to_owned(),
            byte_count,
            body: read_body(path, start, truncated, next, content),
        }
    }

    /// 造一轮只含工具结果的视图轮次。
    fn round(tools: Vec<ToolResult>) -> Round {
        Round {
            assistant: AssistantTurn {
                content: "keep".to_owned(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
            tools,
            usage: ModelUsage {
                prompt_tokens: TokenCount::Unknown,
                completion_tokens: TokenCount::Unknown,
                prompt_cache_hit_tokens: TokenCount::Unknown,
                prompt_cache_miss_tokens: TokenCount::Unknown,
            },
            trailing_user: String::new(),
        }
    }

    #[test]
    fn later_covers_when_full_read_wraps_partial() {
        let later = read_tool("a.rs", 1, false, None, "full", 100, "sha256:full");
        let earlier = read_tool("a.rs", 1, true, Some(41), "part", 50, "sha256:part");
        assert!(later_covers(&later, &earlier));
        assert!(!later_covers(&earlier, &later));
    }

    #[test]
    fn later_covers_rejects_insufficient_overlap() {
        let later = read_tool("a.rs", 10, true, Some(30), "tail", 100, "sha256:tail");
        let earlier = read_tool("a.rs", 1, true, Some(20), "head", 50, "sha256:head");
        assert!(!later_covers(&later, &earlier));
    }

    #[test]
    fn later_covers_rejects_different_path_or_tool() {
        let later = read_tool("a.rs", 1, false, None, "full", 100, "sha256:full");
        let other_path = read_tool("b.rs", 1, false, None, "full", 100, "sha256:other");
        assert!(!later_covers(&later, &other_path));
        let list = ToolResult {
            call_id: "call".to_owned(),
            name: "list_files".to_owned(),
            arguments: "{}".to_owned(),
            result_ref: "sha256:list".to_owned(),
            byte_count: 1,
            body: "tool: list_files\npath: .\npage: 1\n\ndir\ntruncated: false\n".to_owned(),
        };
        assert!(!later_covers(&later, &list));
    }

    #[test]
    fn mask_superseded_rewrites_earlier_when_later_read_covers() {
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-3",
                    900,
                    "sha256:three",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert_eq!(
            view.rounds[0].tools[0].body,
            "[superseded: read_file \"a.rs\" L1-40, 500 B, sha256:one; see round #3 call 1]"
        );
        assert_eq!(
            view.rounds[2].tools[0].body,
            read_body("a.rs", 1, false, None, "full-3")
        );
        assert_eq!(
            edits,
            vec![json!({
                "op": "superseded",
                "round": 1,
                "call": 1,
                "superseded_by_round": 3,
                "superseded_by_call": 1,
                "result_ref": "sha256:one",
            })]
        );
    }

    #[test]
    fn mask_superseded_leaves_protected_rounds_unmasked() {
        let body_r2 = read_body("a.rs", 1, false, None, "full-2");
        let body_r3 = read_body("b.rs", 1, false, None, "other");
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-2",
                    900,
                    "sha256:two",
                )]),
                round(vec![read_tool("b.rs", 1, false, None, "other", 10, "sha256:three")]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert_eq!(edits.len(), 1);
        assert!(view.rounds[0].tools[0].body.starts_with("[superseded:"));
        assert_eq!(view.rounds[2].tools[0].body, body_r2);
        assert_eq!(view.rounds[3].tools[0].body, body_r3);
    }

    #[test]
    fn mask_superseded_no_edit_when_not_covered() {
        let earlier_body = read_body("a.rs", 1, true, Some(41), "chunk-1");
        let later_body = read_body("a.rs", 10, true, Some(30), "tail");
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![]),
                round(vec![read_tool(
                    "a.rs",
                    10,
                    true,
                    Some(30),
                    "tail",
                    100,
                    "sha256:two",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert!(edits.is_empty());
        assert_eq!(view.rounds[0].tools[0].body, earlier_body);
        assert_eq!(view.rounds[2].tools[0].body, later_body);
    }

    #[test]
    fn mask_superseded_uses_latest_covering_read() {
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(51),
                    "chunk-2",
                    600,
                    "sha256:two",
                )]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-3",
                    900,
                    "sha256:three",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["superseded_by_round"], 3);
        assert_eq!(edits[0]["superseded_by_call"], 1);
        assert!(view.rounds[0].tools[0].body.contains("see round #3 call 1"));
    }

    #[test]
    fn mask_superseded_with_two_rounds_leaves_both_reads_unmasked() {
        let earlier_body = read_body("a.rs", 1, true, Some(41), "chunk-1");
        let later_body = read_body("a.rs", 1, false, None, "full-2");
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-2",
                    900,
                    "sha256:two",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert!(edits.is_empty());
        assert_eq!(view.rounds[0].tools[0].body, earlier_body);
        assert_eq!(view.rounds[1].tools[0].body, later_body);
    }

    #[test]
    fn mask_superseded_allows_covering_read_in_protected_round() {
        let full_body = read_body("a.rs", 1, false, None, "full-3");
        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-3",
                    900,
                    "sha256:three",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_superseded(&mut view, &mut edits);
        assert_eq!(edits.len(), 1);
        assert!(view.rounds[0].tools[0].body.starts_with("[superseded:"));
        assert_eq!(view.rounds[2].tools[0].body, full_body);
        assert_eq!(edits[0]["superseded_by_round"], 3);
    }

    /// 三轮布局：第 1 轮部分读，第 3 轮整文件读。
    fn supersede_scenario_view() -> View {
        View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![read_tool(
                    "a.rs",
                    1,
                    true,
                    Some(41),
                    "chunk-1",
                    500,
                    "sha256:one",
                )]),
                round(vec![]),
                round(vec![read_tool(
                    "a.rs",
                    1,
                    false,
                    None,
                    "full-3",
                    900,
                    "sha256:three",
                )]),
            ],
        }
    }

    #[test]
    fn replay_superseded_restores_the_masked_view() {
        let facts = supersede_scenario_view();
        let mut masked = facts.clone();
        let mut edits = Vec::new();
        mask_superseded(&mut masked, &mut edits);
        let mut replayed = facts;
        replay_superseded_edits(&mut replayed, &edits).expect("edits match the facts");
        assert_eq!(replayed, masked);
    }

    #[test]
    fn replay_superseded_keeps_facts_when_edit_does_not_match() {
        let facts = supersede_scenario_view();
        let original = facts.rounds[0].tools[0].body.clone();
        let mut masked = facts.clone();
        let mut edits = Vec::new();
        mask_superseded(&mut masked, &mut edits);

        let mut bad_round = facts.clone();
        let mut bad = edits.clone();
        bad[0]["round"] = json!(9);
        assert_eq!(
            replay_superseded_edits(&mut bad_round, &bad),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_round.rounds[0].tools[0].body, original);

        let mut bad_ref_view = facts.clone();
        let mut bad_ref = edits.clone();
        bad_ref[0]["result_ref"] = json!("sha256:nope");
        assert_eq!(
            replay_superseded_edits(&mut bad_ref_view, &bad_ref),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_ref_view.rounds[0].tools[0].body, original);

        let mut bad_op_view = facts.clone();
        let mut bad_op = edits.clone();
        bad_op[0]["op"] = json!("unchanged");
        assert_eq!(
            replay_superseded_edits(&mut bad_op_view, &bad_op),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_op_view.rounds[0].tools[0].body, original);

        let mut missing_view = facts.clone();
        let mut missing = edits.clone();
        missing[0]
            .as_object_mut()
            .expect("object")
            .remove("superseded_by_round");
        assert_eq!(
            replay_superseded_edits(&mut missing_view, &missing),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(missing_view.rounds[0].tools[0].body, original);
    }

    /// 造一条策略名已经是 `l1@1` 的记录。
    ///
    /// **作用**：策略测试不用经过 `prepare`。
    /// **边界**：`edits` 为空，`hard_limit` 为 0。`apply` 不读这两项。
    fn l1_record() -> ContextRecord {
        ContextRecord {
            schema_version: 1,
            profile_version: 1,
            profile_hash: "profile".to_owned(),
            context_strategy: L1_STRATEGY.to_owned(),
            spec: ContextSpec { hard_limit: 0 },
            estimate: Estimate {
                tokens: 0,
                source: EstimateSource::Estimated,
                anchor_round: None,
            },
            edits: Vec::new(),
            replayable: true,
        }
    }

    #[test]
    fn strategy_round_trips_the_superseded_view() {
        let mut masked = supersede_scenario_view();
        let mut record = l1_record();
        let spec = record.spec.clone();
        SupersedeRead.apply(&mut masked, &spec, &mut record);
        assert_eq!(record.context_strategy, L1_STRATEGY);
        assert_eq!(record.edits.len(), 1);

        let mut replayed = supersede_scenario_view();
        SupersedeRead
            .replay(&mut replayed, &record)
            .expect("l1 record matches the facts");
        assert_eq!(replayed, masked);
    }

    #[test]
    fn strategy_replay_rejects_another_strategy_name() {
        let mut record = l1_record();
        record.context_strategy = L0_STRATEGY.to_owned();
        let mut facts = supersede_scenario_view();
        let original = facts.rounds[0].tools[0].body.clone();
        assert_eq!(
            SupersedeRead.replay(&mut facts, &record),
            Err(ReplayError::UnknownStrategy {
                id: L0_STRATEGY.to_owned(),
            })
        );
        assert_eq!(facts.rounds[0].tools[0].body, original);
    }
}