//! L0 前向去重。
//!
//! **作用**：发现完全相同的新工具结果时，准备把新正文换成存根。
//! **边界**：`ForwardDedup` 实现 `ContextStrategy`。成功和耗尽的 `prepare` 都会调用它。
//!

use crate::record::{ContextRecord, ContextSpec, ReplayError};
use crate::strategy::ContextStrategy;
use crate::transcript::{ToolResult, View};
use serde_json::{json, Value};

/// 写入记录的 L0 策略标识。
pub const L0_STRATEGY: &str = "l0@1";

/// 前向去重策略。
///
/// **作用**：实现 [`ContextStrategy`]。`apply` 替换最后一轮的重复正文，`replay` 按记录写回。
/// **边界**：没有字段。不读取档案，不打开 blob。`prepare` 不调用它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardDedup;

impl ContextStrategy for ForwardDedup {
    /// 返回 `l0@1`。
    ///
    /// **作用**：记录和重放用它认出前向去重。
    /// **边界**：不带估算或档案 hash。
    fn id(&self) -> &str {
        L0_STRATEGY
    }

    /// 把最后一轮里重复的新工具正文换成存根，并追加 edit。
    ///
    /// **作用**：策略链上的 L0 一步。测试直接调用。
    /// **参数**：`view` 只改最后一轮里命中的 `body`，空视图允许。`spec` 不读，硬上限为 0 也允许。`record` 只在 `edits` 末尾追加，已有的编辑保留；`context_strategy` 不改。
    /// **边界**：不删除 blob，不打开 blob。没有命中时不追加。
    fn apply(&self, view: &mut View, _spec: &ContextSpec, record: &mut ContextRecord) {
        mask_latest(view, &mut record.edits);
    }

    /// 按记录把事实视图里的正文写回存根。
    ///
    /// **作用**：校验器以后按策略名调用。测试现在直接调用。
    /// **参数**：`view` 从事实抄出，成功时只改 edit 点到的 `body`。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不是 `l0@1` 时返回 [`ReplayError::UnknownStrategy`]，不改视图。edit 不合法时返回 [`ReplayError::InvalidEdit`]，也不改视图。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        replay_edits(view, &record.edits)
    }
}

/// 把参数文本收成键有序的紧凑形式。
///
/// **作用**：键序不同的同一组参数得到同一个比较键。`same_result` 用它比较两条结果。
/// **参数**：`arguments` 是 `ToolResult::arguments`。空串和非法 JSON 原样返回。
/// **边界**：不解释路径，不读取文件。
fn canonical_arguments(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_owned();
    };
    /// 把一个 JSON 值写成比较用的紧凑文本。
    ///
    /// **作用**：对象按键名排序后递归展开，数组保持原顺序。
    /// **参数**：`value` 是已经解析成功的 JSON。对象、数组和叶子都接受。
    /// **边界**：不解析路径，不读文件。字符串按 `Display` 带引号写出。
    fn write(value: &Value) -> String {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let fields: Vec<String> = keys
                    .into_iter()
                    .map(|key| format!("{key}:{}", write(&map[key])))
                    .collect();
                format!("{{{}}}", fields.join(","))
            }
            Value::Array(items) => {
                let fields: Vec<String> = items.iter().map(write).collect();
                format!("[{}]", fields.join(","))
            }
            other => other.to_string(),
        }
    }
    write(&value)
}

/// 判断两条工具结果是不是同一次内容。
///
/// **作用**：L0 决定要不要把新正文换成存根。
/// **参数**：`left` 和 `right` 是视图里的两条结果。空 `result_ref` 永远不相等。
/// **边界**：不比较 `body`、`call_id` 和 `byte_count`。不打开 blob。
fn same_result(left: &ToolResult, right: &ToolResult) -> bool {
    !left.result_ref.is_empty()
        && left.result_ref == right.result_ref
        && left.name == right.name
        && canonical_arguments(&left.arguments) == canonical_arguments(&right.arguments)
}

/// 把最后一轮里重复的新工具正文换成存根。
///
/// **作用**：`apply` 以后调用它。测试现在直接调用。
/// **参数**：`view` 会被改掉最后一轮里命中的 `body`。空 `rounds` 允许。`edits` 只追加命中的编辑，未命中时保持原内容。
/// **边界**：不改更早轮次，不读 `spec`，不打开 blob。没有命中时不追加。
fn mask_latest(view: &mut View, edits: &mut Vec<Value>) {
    let Some(last) = view.rounds.len().checked_sub(1) else {
        return;
    };
    for call in 0..view.rounds[last].tools.len() {
        let Some((same_round, same_call)) = earliest(view, last, call) else {
            continue;
        };
        let result_ref = view.rounds[last].tools[call].result_ref.clone();
        view.rounds[last].tools[call].body = format!(
            "[unchanged: same result as round #{} call {}]",
            same_round + 1,
            same_call + 1
        );
        edits.push(json!({
            "op": "unchanged",
            "round": last + 1,
            "call": call + 1,
            "same_as_round": same_round + 1,
            "same_as_call": same_call + 1,
            "result_ref": result_ref,
        }));
    }
}

/// 找当前工具结果之前最早的一条相同结果。
///
/// **作用**：`mask_latest` 用返回的下标生成存根和 edit。
/// **参数**：`view` 只读。`round` 和 `call` 是当前结果的下标，从 0 起，必须落在 `view.rounds` 里。
/// **边界**：不包含当前这条。没有更早的相同结果时返回 `None`。
fn earliest(view: &View, round: usize, call: usize) -> Option<(usize, usize)> {
    for earlier_round in 0..=round {
        let limit = if earlier_round == round {
            call
        } else {
            view.rounds[earlier_round].tools.len()
        };
        for earlier_call in 0..limit {
            if same_result(
                &view.rounds[round].tools[call],
                &view.rounds[earlier_round].tools[earlier_call],
            ) {
                return Some((earlier_round, earlier_call));
            }
        }
    }
    None
}

/// 按 edit 把事实视图里的工具正文写回存根。
///
/// **作用**：重放测试直接调用。以后的 `replay` 再转调它。
/// **参数**：`view` 是从事实抄出的视图，成功时只改 edit 点到的 `body`。`edits` 是 `mask_latest` 追加的那些值，空切片允许。
/// **边界**：先检查全部 edit，任一不合法就返回 [`ReplayError::InvalidEdit`]，并且不改 `view`。不重新比较工具结果，不打开 blob。
fn replay_edits(view: &mut View, edits: &[Value]) -> Result<(), ReplayError> {
    let mut changes = Vec::new();
    for edit in edits {
        let Some(object) = edit.as_object() else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(op) = object.get("op").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        if op != "unchanged" {
            return Err(ReplayError::InvalidEdit);
        }
        let round = one_based(object, "round")?;
        let call = one_based(object, "call")?;
        let same_round = one_based(object, "same_as_round")?;
        let same_call = one_based(object, "same_as_call")?;
        let Some(result_ref) = object.get("result_ref").and_then(Value::as_str) else {
            return Err(ReplayError::InvalidEdit);
        };
        let Some(tool) = tool_at(view, round, call) else {
            return Err(ReplayError::InvalidEdit);
        };
        if tool_at(view, same_round, same_call).is_none() || tool.result_ref != result_ref {
            return Err(ReplayError::InvalidEdit);
        }
        changes.push((
            round,
            call,
            format!(
                "[unchanged: same result as round #{} call {}]",
                same_round + 1,
                same_call + 1
            ),
        ));
    }
    for (round, call, body) in changes {
        view.rounds[round].tools[call].body = body;
    }
    Ok(())
}

/// 只重放 `op` 为 `unchanged` 的 edit。
///
/// **作用**：`l0l1@1` 先调用它，再重放作废遮蔽。
/// **参数**：`view` 从事实抄出，成功时只改这些 edit 点到的 `body`。`edits` 只能含 `unchanged`，空切片允许。
/// **边界**：出现别的 `op` 时返回 [`ReplayError::InvalidEdit`]，并且不改 `view`。
pub(crate) fn replay_unchanged_edits(
    view: &mut View,
    edits: &[Value],
) -> Result<(), ReplayError> {
    replay_edits(view, edits)
}

/// 读取 edit 里从 1 起的序号，并转换成切片下标。
///
/// **作用**：`replay_edits` 用它读 `round`、`call`、`same_as_round` 和 `same_as_call`。
/// **参数**：`object` 是一条 edit。`key` 是要读的字段名。值必须是不小于 1 的整数。
/// **边界**：缺字段、负数、小数或 0 都返回 [`ReplayError::InvalidEdit`]。这里不检查视图里有没有这个位置。
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
/// **作用**：`replay_edits` 用它确认序号落在视图里。
/// **参数**：`view` 只读。`round` 是轮次下标，`call` 是该轮工具下标，都从 0 起。
/// **边界**：越界时返回 `None`。不检查 `result_ref`。
fn tool_at(view: &View, round: usize, call: usize) -> Option<&ToolResult> {
    view.rounds.get(round).and_then(|item| item.tools.get(call))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{ModelUsage, TokenCount};

    use crate::record::{Estimate, EstimateSource};
    use crate::transcript::{AssistantTurn, Round};

    #[test]
    fn id_is_l0() {
        assert_eq!(ForwardDedup.id(), "l0@1");
    }

    /// 造一条只用于相等判断的工具结果。
    ///
    /// **作用**：让测试只改名字、参数和引用，其余字段保持无关。
    /// **参数**：`name` 是工具名，允许空串。`arguments` 是参数文本，允许非法 JSON。`result_ref` 是正文引用，空串表示没有可比较的内容。
    /// **边界**：`call_id` 固定为 `different`，`byte_count` 为 0，`body` 为空。不打开 blob。
    fn sample(name: &str, arguments: &str, result_ref: &str) -> ToolResult {
        ToolResult {
            call_id: "different".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            result_ref: result_ref.to_owned(),
            byte_count: 0,
            body: String::new(),
        }
    }

    #[test]
    fn same_result_requires_name_arguments_and_ref() {
        let left = sample(
            "read_file",
            r#"{"path":"a.rs","start_line":1}"#,
            "sha256:abc",
        );
        let swapped = sample(
            "read_file",
            r#"{"start_line":1,"path":"a.rs"}"#,
            "sha256:abc",
        );
        assert!(same_result(&left, &swapped));
        assert!(!same_result(
            &left,
            &sample(
                "list_files",
                r#"{"path":"a.rs","start_line":1}"#,
                "sha256:abc"
            )
        ));
        assert!(!same_result(
            &left,
            &sample(
                "read_file",
                r#"{"path":"b.rs","start_line":1}"#,
                "sha256:abc"
            )
        ));
        assert!(!same_result(
            &left,
            &sample(
                "read_file",
                r#"{"path":"a.rs","start_line":1}"#,
                "sha256:def"
            )
        ));
        assert!(!same_result(
            &sample("read_file", r#"{"path":"a.rs"}"#, ""),
            &sample("read_file", r#"{"path":"a.rs"}"#, ""),
        ));
    }

    /// 在 `sample` 上填入工具正文。
    ///
    /// **作用**：替换测试要断言正文是否被改掉。
    /// **参数**：`name`、`arguments`、`result_ref` 的含义与 `sample` 相同。`body` 是替换前的正文，允许空串。
    /// **边界**：`call_id` 仍是 `different`，`byte_count` 仍是 0。
    fn tool(name: &str, arguments: &str, result_ref: &str, body: &str) -> ToolResult {
        let mut result = sample(name, arguments, result_ref);
        result.body = body.to_owned();
        result
    }

    /// 造一轮只含工具结果的视图轮次。
    ///
    /// **作用**：让替换测试不用填写用量和 assistant 细节。
    /// **参数**：`tools` 按响应里的调用顺序排列，空 `Vec` 表示这一轮没有工具。
    /// **边界**：assistant 正文固定为 `keep`。用量四项都是 `Unknown`。
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
    fn mask_latest_rewrites_only_the_new_duplicate_bodies() {
        let mut edits = Vec::new();
        let mut empty = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: Vec::new(),
        };
        mask_latest(&mut empty, &mut edits);
        assert!(edits.is_empty());

        let mut view = View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![
                    tool("read_file", r#"{"path":"a.rs"}"#, "sha256:abc", "old-a"),
                    tool(
                        "read_file",
                        r#"{"path":"b.rs","start_line":1}"#,
                        "sha256:bbb",
                        "old-b",
                    ),
                ]),
                round(vec![
                    tool(
                        "read_file",
                        r#"{"path":"b.rs","start_line":1}"#,
                        "sha256:bbb",
                        "new-b",
                    ),
                    tool(
                        "read_file",
                        r#"{"start_line":1,"path":"b.rs"}"#,
                        "sha256:bbb",
                        "again",
                    ),
                    tool("read_file", r#"{"path":"c.rs"}"#, "sha256:ccc", "fresh"),
                    tool("search", "{}", "sha256:same", "first"),
                    tool("search", "{}", "sha256:same", "second"),
                ]),
            ],
        };
        mask_latest(&mut view, &mut edits);

        assert_eq!(view.rounds[0].tools[0].body, "old-a");
        assert_eq!(view.rounds[0].tools[1].body, "old-b");
        assert_eq!(view.rounds[0].assistant.content, "keep");
        assert_eq!(
            view.rounds[1].tools[0].body,
            "[unchanged: same result as round #1 call 2]"
        );
        assert_eq!(
            view.rounds[1].tools[1].body,
            "[unchanged: same result as round #1 call 2]"
        );
        assert_eq!(view.rounds[1].tools[2].body, "fresh");
        assert_eq!(view.rounds[1].tools[3].body, "first");
        assert_eq!(
            view.rounds[1].tools[4].body,
            "[unchanged: same result as round #2 call 4]"
        );
        assert_eq!(view.rounds[1].tools[0].call_id, "different");
        assert_eq!(
            edits,
            vec![
                json!({
                    "op": "unchanged",
                    "round": 2,
                    "call": 1,
                    "same_as_round": 1,
                    "same_as_call": 2,
                    "result_ref": "sha256:bbb",
                }),
                json!({
                    "op": "unchanged",
                    "round": 2,
                    "call": 2,
                    "same_as_round": 1,
                    "same_as_call": 2,
                    "result_ref": "sha256:bbb",
                }),
                json!({
                    "op": "unchanged",
                    "round": 2,
                    "call": 5,
                    "same_as_round": 2,
                    "same_as_call": 4,
                    "result_ref": "sha256:same",
                }),
            ]
        );
    }

    /// 造一份只有一条重复结果的事实视图。
    ///
    /// **作用**：重放测试共用同一份原文。
    /// **边界**：第 1 轮正文是 `old`，第 2 轮正文是 `new`。引用都是 `sha256:abc`。
    fn duplicate_view() -> View {
        View {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![
                round(vec![tool(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    "sha256:abc",
                    "old",
                )]),
                round(vec![tool(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    "sha256:abc",
                    "new",
                )]),
            ],
        }
    }

    #[test]
    fn replay_edits_restores_the_masked_view() {
        let mut masked = duplicate_view();
        let mut edits = Vec::new();
        mask_latest(&mut masked, &mut edits);
        let mut replayed = duplicate_view();
        replay_edits(&mut replayed, &edits).expect("edits match the facts");
        assert_eq!(replayed, masked);
    }

    #[test]
    fn replay_edits_keeps_the_facts_when_an_edit_does_not_match() {
        let mut masked = duplicate_view();
        let mut edits = Vec::new();
        mask_latest(&mut masked, &mut edits);
        let mut facts = duplicate_view();
        let mut bad_round = edits.clone();
        bad_round[0]["round"] = json!(9);
        assert_eq!(
            replay_edits(&mut facts, &bad_round),
            Err(ReplayError::InvalidEdit)
        );
        let mut bad_ref = edits.clone();
        bad_ref[0]["result_ref"] = json!("sha256:nope");
        assert_eq!(
            replay_edits(&mut facts, &bad_ref),
            Err(ReplayError::InvalidEdit)
        );
        let mut bad_op = edits.clone();
        bad_op[0]["op"] = json!("mask");
        assert_eq!(
            replay_edits(&mut facts, &bad_op),
            Err(ReplayError::InvalidEdit)
        );
        let mut missing = edits.clone();
        missing[0].as_object_mut().expect("object").remove("call");
        assert_eq!(
            replay_edits(&mut facts, &missing),
            Err(ReplayError::InvalidEdit)
        );
        assert_eq!(facts.rounds[1].tools[0].body, "new");
    }

    /// 造一条策略名已经是 `l0@1` 的记录。
    ///
    /// **作用**：策略测试不用经过 `prepare`。
    /// **边界**：`edits` 为空，`hard_limit` 为 0。`apply` 不读这两项。
    fn l0_record() -> ContextRecord {
        ContextRecord {
            schema_version: 1,
            profile_version: 1,
            profile_hash: "profile".to_owned(),
            context_strategy: L0_STRATEGY.to_owned(),
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
    fn strategy_round_trips_the_masked_view() {
        let mut masked = duplicate_view();
        let mut record = l0_record();
        let spec = record.spec.clone();
        ForwardDedup.apply(&mut masked, &spec, &mut record);
        assert_eq!(record.context_strategy, L0_STRATEGY);
        assert_eq!(record.spec.hard_limit, 0);

        let mut replayed = duplicate_view();
        ForwardDedup
            .replay(&mut replayed, &record)
            .expect("l0 record matches the facts");
        assert_eq!(replayed, masked);
    }

    #[test]
    fn strategy_replay_rejects_another_strategy_name() {
        let mut record = l0_record();
        record.context_strategy = "identity@1".to_owned();
        let mut facts = duplicate_view();
        assert_eq!(
            ForwardDedup.replay(&mut facts, &record),
            Err(ReplayError::UnknownStrategy {
                id: "identity@1".to_owned(),
            })
        );
        assert_eq!(facts.rounds[1].tools[0].body, "new");
    }
}
