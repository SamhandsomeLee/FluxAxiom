//! L2 陈旧遮蔽：遮蔽保护窗口外的工具原文，并按 `l2@1` 重放。
//!
//! **作用**：[`StaleMask`] 实现策略。[`ContextManager::prepare`] 在超过软触发且仍高于目标时调用它。
//! [`ContextManager::replay`] 按 `l2@1`、`l0l2@1` 或 `l0l1l2@1` 重放。
//! **边界**：不读档案，不打开 blob。软触发和腾出量在 `prepare` 里。

use crate::record::{ContextRecord, ContextSpec, ReplayError};
use crate::strategy::ContextStrategy;
use crate::supersede::PROTECT_ROUNDS;
use crate::transcript::{ToolResult, View};
use serde_json::{json, Value};

/// 写入记录的 L2 策略标识。
pub const L2_STRATEGY: &str = "l2@1";

/// 陈旧遮蔽策略。
///
/// **作用**：实现 [`ContextStrategy`]。`apply` 遮掉保护窗口外的工具原文，`replay` 按记录写回。
/// **边界**：没有字段。不读档案，不打开 blob。是否留下由 `prepare` 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleMask;

impl ContextStrategy for StaleMask {
    /// 返回 `l2@1`。
    ///
    /// **作用**：记录和重放用它认出陈旧遮蔽。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L2_STRATEGY
    }

    /// 把保护窗口外的工具原文换成存根，并追加 edit。
    ///
    /// **作用**：`prepare` 在决定留下 L2 之前调用。策略测试也可以直接调用。
    /// **参数**：`view` 只改窗口外、正文仍是原文、且不是本轮 L0 指针目标的 `body`。`spec` 不读，硬上限为 0 也允许。
    /// `record` 只在 `edits` 末尾追加，不改策略名。已有的 `unchanged` edit 用来认出指针目标。
    /// **边界**：没有命中时不追加。不删除 blob。
    fn apply(&self, view: &mut View, _spec: &ContextSpec, record: &mut ContextRecord) {
        mask_stale(view, &mut record.edits);
    }

    /// 按记录把事实视图里的正文写回存根。
    ///
    /// **作用**：`ContextManager::replay` 在策略名是 `l2@1` 时调用。
    /// **参数**：`view` 从事实抄出。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l2@1` 时返回 [`ReplayError::UnknownStrategy`]，不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        replay_stale_edits(view, &record.edits)
    }
}

/// `prepare` 在留下 L2、但没有留下 L1 时写入的策略标识。
pub const L0L2_STRATEGY: &str = "l0l2@1";

/// 先重放 L0 的 edit，再重放 L2 的 edit。
///
/// **作用**：`ContextManager::replay` 在策略名是 `l0l2@1` 时调用。
/// **边界**：`prepare` 不调用 [`Self::apply`]。不打开 blob。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L0ThenL2;

impl ContextStrategy for L0ThenL2 {
    /// 返回 `l0l2@1`。
    ///
    /// **作用**：记录和重放用它认出「L0 之后又留下了 L2」。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L0L2_STRATEGY
    }

    /// 不在这里跑目标判断。
    ///
    /// **作用**：trait 要求有 `apply`。目标和腾出量在 `prepare` 里。
    /// **参数**：`view`、`spec`、`record` 都不读。空视图允许。
    /// **边界**：不改视图，不追加 edit。
    fn apply(&self, _view: &mut View, _spec: &ContextSpec, _record: &mut ContextRecord) {}

    /// 按 `op` 分成两段重放。
    ///
    /// **作用**：同一条记录里可以同时有 `unchanged` 和 `stale`。
    /// **参数**：`view` 从事实抄出。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l0l2@1`，或出现第三种 `op`，都不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        let mut unchanged = Vec::new();
        let mut stale = Vec::new();
        for edit in &record.edits {
            let Some(op) = edit.get("op").and_then(|value| value.as_str()) else {
                return Err(ReplayError::InvalidEdit);
            };
            match op {
                "unchanged" => unchanged.push(edit.clone()),
                "stale" => stale.push(edit.clone()),
                _ => return Err(ReplayError::InvalidEdit),
            }
        }
        crate::forward_dedup::replay_unchanged_edits(view, &unchanged)?;
        replay_stale_edits(view, &stale)
    }
}

/// `prepare` 在 L1 和 L2 都留下时写入的策略标识。
pub const L0L1L2_STRATEGY: &str = "l0l1l2@1";

/// 先重放 L0，再重放 L1，最后重放 L2。
///
/// **作用**：`ContextManager::replay` 在策略名是 `l0l1l2@1` 时调用。
/// **边界**：`prepare` 不调用 [`Self::apply`]。不打开 blob。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L0ThenL1ThenL2;

impl ContextStrategy for L0ThenL1ThenL2 {
    /// 返回 `l0l1l2@1`。
    ///
    /// **作用**：记录和重放用它认出「L0、L1、L2 都留下了」。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L0L1L2_STRATEGY
    }

    /// 不在这里跑软触发或目标判断。
    ///
    /// **作用**：trait 要求有 `apply`。去留在 `prepare` 里。
    /// **参数**：`view`、`spec`、`record` 都不读。空视图允许。
    /// **边界**：不改视图，不追加 edit。
    fn apply(&self, _view: &mut View, _spec: &ContextSpec, _record: &mut ContextRecord) {}

    /// 按 `op` 分成三段重放。
    ///
    /// **作用**：同一条记录里可以同时有 `unchanged`、`superseded` 和 `stale`。
    /// **参数**：`view` 从事实抄出。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l0l1l2@1`，或出现第四种 `op`，都不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        let mut unchanged = Vec::new();
        let mut superseded = Vec::new();
        let mut stale = Vec::new();
        for edit in &record.edits {
            let Some(op) = edit.get("op").and_then(|value| value.as_str()) else {
                return Err(ReplayError::InvalidEdit);
            };
            match op {
                "unchanged" => unchanged.push(edit.clone()),
                "superseded" => superseded.push(edit.clone()),
                "stale" => stale.push(edit.clone()),
                _ => return Err(ReplayError::InvalidEdit),
            }
        }
        crate::forward_dedup::replay_unchanged_edits(view, &unchanged)?;
        crate::supersede::replay_superseded_edits(view, &superseded)?;
        replay_stale_edits(view, &stale)
    }
}

/// 把保护窗口之外、正文仍是原文的工具结果换成陈旧存根。
///
/// **作用**：L2 的遮蔽函数。测试直接调用。
/// **参数**：`view` 只改命中的工具 `body`，空视图允许。`edits` 只在末尾追加命中的 edit，已有内容保留。
/// **边界**：最近 [`PROTECT_ROUNDS`] 个模型轮不动。正文已以 `[unchanged:`、`[superseded:` 或 `[stale:` 开头时跳过。
/// 已有 `unchanged` edit 的 `same_as_round` / `same_as_call` 指向的原文也跳过。缺这两个字段的 edit 不当成指针。
/// 不改 `system`、`user`、assistant 和 `trailing_user`。不读规格，不改策略名，不打开 blob。
/// 从最旧一轮的最早一次调用开始，窗口外的原文一批全部替换，不按目标中途停下。
fn mask_stale(view: &mut View, edits: &mut Vec<Value>) {
    let pointed = l0_pointer_targets(edits);
    let protect_from = view.rounds.len().saturating_sub(PROTECT_ROUNDS);
    for round in 0..protect_from {
        let tool_count = view.rounds[round].tools.len();
        for call in 0..tool_count {
            if pointed.contains(&(round, call))
                || is_existing_stub(&view.rounds[round].tools[call].body)
            {
                continue;
            }
            let tool = view.rounds[round].tools[call].clone();
            view.rounds[round].tools[call].body = stale_stub(&tool);
            edits.push(json!({
                "op": "stale",
                "round": round + 1,
                "call": call + 1,
                "result_ref": tool.result_ref,
            }));
        }
    }
}

/// 收集已有 L0 指针指向的工具位置。
///
/// **作用**：`mask_stale` 避免把「与第 N 轮相同」指到的原文再换成陈旧存根。
/// **参数**：`edits` 是调用前已经写下的 edit。空切片表示没有指针。
/// **边界**：只认 `op` 为 `unchanged` 且 `same_as_round`、`same_as_call` 都是从 1 起的整数。其它 edit 忽略。
fn l0_pointer_targets(edits: &[Value]) -> Vec<(usize, usize)> {
    let mut targets = Vec::new();
    for edit in edits {
        let Some(object) = edit.as_object() else {
            continue;
        };
        if object.get("op").and_then(Value::as_str) != Some("unchanged") {
            continue;
        }
        let Ok(round) = one_based(object, "same_as_round") else {
            continue;
        };
        let Ok(call) = one_based(object, "same_as_call") else {
            continue;
        };
        targets.push((round, call));
    }
    targets
}

/// 判断这段正文是不是已有的存根。
///
/// **作用**：`mask_stale` 用来跳过 L0、L1 和已经陈旧遮蔽过的正文。
/// **参数**：`body` 是一条工具结果的正文。空串不是存根。
/// **边界**：只认这三个前缀，不解析存根里面的字段。
fn is_existing_stub(body: &str) -> bool {
    body.starts_with("[unchanged:")
        || body.starts_with("[superseded:")
        || body.starts_with("[stale:")
}

/// 用工具名、参数、大小和引用拼一条陈旧存根。
///
/// **作用**：`mask_stale` 写入 `body`。以后的重放也用同一句，不把存根原文放进 edit。
/// **参数**：`tool` 提供 `name`、`arguments`、`byte_count` 和 `result_ref`。这四项都允许空串或 0。
/// **边界**：不读 `body`，不打开 `result_ref`，也不重新计算 hash。
fn stale_stub(tool: &ToolResult) -> String {
    format!(
        "[stale: {} {}, {} B, {}; call this tool again if needed]",
        tool.name, tool.arguments, tool.byte_count, tool.result_ref
    )
}

/// 按 L2 edit 把事实视图里的正文写回存根。
///
/// **作用**：重放测试直接调用。[`StaleMask::replay`] 再转调它。
/// **参数**：`view` 从事实抄出，成功时只改 edit 点到的 `body`。`edits` 只处理 `op: stale`，空切片允许。
/// **边界**：先检查全部 edit，任一不合法返回 [`ReplayError::InvalidEdit`] 且不改 `view`。
/// 存根由该条工具的名字、参数、大小和 `result_ref` 重拼，不读取 edit 里的其它文字。
/// 轮次落在最近 [`PROTECT_ROUNDS`] 轮内时也不合法。不打开 blob。
fn replay_stale_edits(view: &mut View, edits: &[Value]) -> Result<(), ReplayError> {
    let mut changes = Vec::new();
    let protect_from = view.rounds.len().saturating_sub(PROTECT_ROUNDS);
    for edit in edits {
        let Some(object) = edit.as_object() else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(op) = object.get("op").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        if op != "stale" {
            return Err(ReplayError::InvalidEdit);
        }
        let round = one_based(object, "round")?;
        let call = one_based(object, "call")?;
        let Some(result_ref) = object.get("result_ref").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(tool) = tool_at(view, round, call) else {
            return Err(ReplayError::InvalidEdit);
        };
        if tool.result_ref != result_ref || round >= protect_from {
            return Err(ReplayError::InvalidEdit);
        }
        changes.push((round, call, stale_stub(tool)));
    }
    for (round, call, body) in changes {
        view.rounds[round].tools[call].body = body;
    }
    Ok(())
}

/// 读取 edit 里从 1 起的序号，并转换成切片下标。
///
/// **作用**：`replay_stale_edits` 读轮次与调用序号。
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
/// **作用**：`replay_stale_edits` 确认序号落在视图里。
/// **参数**：`view` 只读。`round` 是轮次下标，`call` 是该轮工具下标，都从 0 起。
/// **边界**：越界时返回 `None`。
fn tool_at(view: &View, round: usize, call: usize) -> Option<&ToolResult> {
    view.rounds.get(round).and_then(|item| item.tools.get(call))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{ModelUsage, TokenCount};

    use crate::forward_dedup::L0_STRATEGY;
    use crate::manager::ContextManager;
    use crate::record::{Estimate, EstimateSource};
    use crate::transcript::{AssistantTurn, Round, Transcript};

    /// 造一条工具结果。
    ///
    /// **作用**：陈旧遮蔽测试共用。
    /// **参数**：`name` 是工具名。`arguments` 是参数文本，原样进存根。`body` 是原文。
    /// `byte_count` 写入存根的大小。`result_ref` 写入存根和 edit，测试里用 `sha256:` 加短名。
    /// **边界**：`call_id` 固定为 `call`。不渲染 `read_file` 的路径行。
    fn tool(
        name: &str,
        arguments: &str,
        body: &str,
        byte_count: u64,
        result_ref: &str,
    ) -> ToolResult {
        ToolResult {
            call_id: "call".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            result_ref: result_ref.to_owned(),
            byte_count,
            body: body.to_owned(),
        }
    }

    /// 造一轮只含工具结果的视图轮次。
    ///
    /// **作用**：让 assistant 正文固定为 `keep`，用来确认遮蔽没有改到它。
    /// **参数**：`tools` 按调用顺序放入这一轮。空 `Vec` 表示这一轮没有工具。
    /// **边界**：思维链和 `trailing_user` 为空。用量四项都是未知。
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

    /// 三轮时只遮第 1 轮的原文，并追加一条 edit。
    ///
    /// **作用**：锁住存根句子、edit 字段，以及保护窗口和 system、user、assistant 不动。
    /// **边界**：第 2、3 轮落在保护窗口内。
    #[test]
    fn mask_stale_rewrites_the_oldest_round_outside_the_window() {
        let middle = "middle-body";
        let newest = "newest-body";
        let mut view = View {
            system: "sys".to_owned(),
            user: "usr".to_owned(),
            rounds: vec![
                round(vec![tool(
                    "list_files",
                    r#"{"path":"."}"#,
                    "dir-listing",
                    12,
                    "sha256:abc",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    middle,
                    20,
                    "sha256:mid",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"b.rs"}"#,
                    newest,
                    30,
                    "sha256:new",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_stale(&mut view, &mut edits);
        assert_eq!(
            view.rounds[0].tools[0].body,
            "[stale: list_files {\"path\":\".\"}, 12 B, sha256:abc; call this tool again if needed]"
        );
        assert_eq!(view.rounds[1].tools[0].body, middle);
        assert_eq!(view.rounds[2].tools[0].body, newest);
        assert_eq!(view.system, "sys");
        assert_eq!(view.user, "usr");
        assert_eq!(view.rounds[0].assistant.content, "keep");
        assert_eq!(view.rounds[1].assistant.content, "keep");
        assert_eq!(view.rounds[2].assistant.content, "keep");
        assert_eq!(
            edits,
            vec![json!({
                "op": "stale",
                "round": 1,
                "call": 1,
                "result_ref": "sha256:abc",
            })]
        );
    }

    /// 只有两轮时，两轮都在保护窗口内，不产生 edit。
    ///
    /// **作用**：确认窗口盖住全部轮次时正文保持原文。
    /// **边界**：不把较旧的那一轮当成窗口外。
    #[test]
    fn mask_stale_leaves_two_rounds_unmasked() {
        let older = "older-body";
        let newer = "newer-body";
        let mut view = View {
            system: "sys".to_owned(),
            user: "usr".to_owned(),
            rounds: vec![
                round(vec![tool(
                    "list_files",
                    r#"{"path":"."}"#,
                    older,
                    12,
                    "sha256:abc",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    newer,
                    20,
                    "sha256:mid",
                )]),
            ],
        };
        let mut edits = Vec::new();
        mask_stale(&mut view, &mut edits);
        assert!(edits.is_empty());
        assert_eq!(view.rounds[0].tools[0].body, older);
        assert_eq!(view.rounds[1].tools[0].body, newer);
    }

    /// 三轮各一条工具，只有第 1 轮在保护窗口外。
    ///
    /// **作用**：重放测试共用的事实视图。
    /// **边界**：assistant 正文是 `keep`。第 2、3 轮正文分别是 `middle-body` 和 `newest-body`。
    fn three_round_view() -> View {
        View {
            system: "sys".to_owned(),
            user: "usr".to_owned(),
            rounds: vec![
                round(vec![tool(
                    "list_files",
                    r#"{"path":"."}"#,
                    "dir-listing",
                    12,
                    "sha256:abc",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    "middle-body",
                    20,
                    "sha256:mid",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"b.rs"}"#,
                    "newest-body",
                    30,
                    "sha256:new",
                )]),
            ],
        }
    }

    /// 从事实重放后，视图与 `mask_stale` 的结果相同。
    ///
    /// **作用**：确认存根来自工具名、参数、大小和 `result_ref`。
    /// **边界**：edit 里另写的 `body` 不进入视图。
    #[test]
    fn replay_stale_restores_the_masked_view() {
        let facts = three_round_view();
        let mut masked = facts.clone();
        let mut edits = Vec::new();
        mask_stale(&mut masked, &mut edits);
        edits[0]["body"] = json!("tampered stub");
        let mut replayed = facts;
        replay_stale_edits(&mut replayed, &edits).expect("edits match the facts");
        assert_eq!(replayed, masked);
    }

    /// 任一条 edit 对不上事实时，整批拒绝，正文保持原文。
    ///
    /// **作用**：锁住序号、引用、`op` 和缺字段。
    /// **边界**：第一条即使合法，后面有一条不合法时也不写入。
    #[test]
    fn replay_stale_keeps_facts_when_an_edit_is_invalid() {
        let facts = three_round_view();
        let original = facts.rounds[0].tools[0].body.clone();
        let mut masked = facts.clone();
        let mut edits = Vec::new();
        mask_stale(&mut masked, &mut edits);

        let mut bad_round = facts.clone();
        let mut bad = edits.clone();
        bad[0]["round"] = json!(9);
        assert_eq!(
            replay_stale_edits(&mut bad_round, &bad),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_round.rounds[0].tools[0].body, original);

        let mut bad_ref_view = facts.clone();
        let mut bad_ref = edits.clone();
        bad_ref[0]["result_ref"] = json!("sha256:nope");
        assert_eq!(
            replay_stale_edits(&mut bad_ref_view, &bad_ref),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_ref_view.rounds[0].tools[0].body, original);

        let mut bad_op_view = facts.clone();
        let mut bad_op = edits.clone();
        bad_op[0]["op"] = json!("unchanged");
        assert_eq!(
            replay_stale_edits(&mut bad_op_view, &bad_op),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(bad_op_view.rounds[0].tools[0].body, original);

        let mut missing_view = facts.clone();
        let mut missing = edits.clone();
        missing[0]
            .as_object_mut()
            .expect("object")
            .remove("result_ref");
        assert_eq!(
            replay_stale_edits(&mut missing_view, &missing),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(missing_view.rounds[0].tools[0].body, original);

        let mut batch = facts.clone();
        batch.rounds[0].tools.push(tool(
            "search_code",
            r#"{"query":"x"}"#,
            "hits",
            4,
            "sha256:hits",
        ));
        let first = batch.rounds[0].tools[0].body.clone();
        let second = batch.rounds[0].tools[1].body.clone();
        let edits = vec![
            json!({
                "op": "stale",
                "round": 1,
                "call": 1,
                "result_ref": "sha256:abc",
            }),
            json!({
                "op": "stale",
                "round": 1,
                "call": 2,
                "result_ref": "sha256:nope",
            }),
        ];
        assert_eq!(
            replay_stale_edits(&mut batch, &edits),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(batch.rounds[0].tools[0].body, first);
        assert_eq!(batch.rounds[0].tools[1].body, second);
    }

    /// 手工写下保护窗口内的编辑时，重放拒绝并且不改正文。
    ///
    /// **作用**：确认重放自己检查窗口，不信任 edit 的位置。
    /// **边界**：第 3 轮落在最近两轮内。`result_ref` 与事实一致，仍拒绝。
    #[test]
    fn replay_stale_rejects_a_protected_round() {
        let mut facts = three_round_view();
        let original = facts.rounds[2].tools[0].body.clone();
        let edits = vec![json!({
            "op": "stale",
            "round": 3,
            "call": 1,
            "result_ref": "sha256:new",
        })];
        assert_eq!(
            replay_stale_edits(&mut facts, &edits),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(facts.rounds[2].tools[0].body, original);
    }

    /// 造一条策略名已经是 `l2@1` 的记录。
    ///
    /// **作用**：策略测试不用经过 `prepare`。
    /// **边界**：`edits` 为空，`hard_limit` 为 0。`apply` 不读这两项。
    fn l2_record() -> ContextRecord {
        ContextRecord {
            schema_version: 1,
            profile_version: 1,
            profile_hash: "profile".to_owned(),
            context_strategy: L2_STRATEGY.to_owned(),
            spec: ContextSpec::schema1(0),
            estimate: Estimate {
                tokens: 0,
                source: EstimateSource::Estimated,
                anchor_round: None,
            },
            edits: Vec::new(),
            replayable: true,
        }
    }

    /// 把三轮视图收成一份事实。
    ///
    /// **作用**：`ContextManager::replay` 要的是 [`Transcript`]，不是 [`View`]。
    /// **边界**：正文与 [`three_round_view`] 相同。
    fn three_round_transcript() -> Transcript {
        let view = three_round_view();
        Transcript {
            system: view.system,
            user: view.user,
            rounds: view.rounds,
        }
    }

    /// `apply` 留下的 edit 能被 `l2@1` 重放成同一份视图。
    ///
    /// **作用**：锁住策略名在 apply 期间不变，并且管理器按这个名字选出实现。
    /// **边界**：不经过 `prepare`。
    #[test]
    fn strategy_round_trips_the_stale_view() {
        let mut masked = three_round_view();
        let mut record = l2_record();
        let spec = record.spec.clone();
        StaleMask.apply(&mut masked, &spec, &mut record);
        assert_eq!(record.context_strategy, L2_STRATEGY);
        assert_eq!(record.edits.len(), 1);

        let mut replayed = three_round_view();
        StaleMask
            .replay(&mut replayed, &record)
            .expect("l2 record matches the facts");
        assert_eq!(replayed, masked);

        let transcript = three_round_transcript();
        let managed = ContextManager::replay(&transcript, &record).expect("manager accepts l2");
        assert_eq!(managed, masked);
    }

    /// 策略名不是 `l2@1` 时，重放拒绝并且不改正文。
    ///
    /// **作用**：避免把别的策略的 edit 当成陈旧遮蔽。
    /// **边界**：用 `l0@1` 作为错误名字。视图保持原文。
    #[test]
    fn strategy_replay_rejects_another_strategy_name() {
        let mut record = l2_record();
        record.context_strategy = L0_STRATEGY.to_owned();
        let mut facts = three_round_view();
        let original = facts.rounds[0].tools[0].body.clone();
        assert_eq!(
            StaleMask.replay(&mut facts, &record),
            Err(ReplayError::UnknownStrategy {
                id: L0_STRATEGY.to_owned(),
            })
        );
        assert_eq!(facts.rounds[0].tools[0].body, original);
    }

    /// 已有 L0 指针时，不遮它指向的原文，仍遮窗口外的另一条。
    ///
    /// **作用**：锁住 `same_as_round` / `same_as_call`。
    /// **边界**：四轮里前两轮在保护窗口外。第 1 轮是指针目标。
    #[test]
    fn mask_stale_skips_the_l0_pointer_target() {
        let original = "original-body";
        let other = "other-body";
        let mut view = View {
            system: "sys".to_owned(),
            user: "usr".to_owned(),
            rounds: vec![
                round(vec![tool(
                    "list_files",
                    r#"{"path":"."}"#,
                    original,
                    12,
                    "sha256:same",
                )]),
                round(vec![tool(
                    "list_files",
                    r#"{"path":"src"}"#,
                    other,
                    8,
                    "sha256:other",
                )]),
                round(vec![tool(
                    "list_files",
                    r#"{"path":"near"}"#,
                    "near",
                    4,
                    "sha256:near",
                )]),
                round(vec![tool(
                    "list_files",
                    r#"{"path":"."}"#,
                    original,
                    12,
                    "sha256:same",
                )]),
            ],
        };
        let mut edits = vec![json!({
            "op": "unchanged",
            "round": 4,
            "call": 1,
            "same_as_round": 1,
            "same_as_call": 1,
            "result_ref": "sha256:same",
        })];
        mask_stale(&mut view, &mut edits);
        assert_eq!(view.rounds[0].tools[0].body, original);
        assert_eq!(
            view.rounds[1].tools[0].body,
            "[stale: list_files {\"path\":\"src\"}, 8 B, sha256:other; call this tool again if needed]"
        );
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[1]["op"], "stale");
        assert_eq!(edits[1]["round"], 2);
    }
}
