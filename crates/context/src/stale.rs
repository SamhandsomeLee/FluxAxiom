//! L2 陈旧遮蔽的第一刀：把保护窗口之外的工具原文换成存根。
//!
//! **作用**：测试直接调用 [`mask_stale`]。`prepare` 这一步不调用它。
//! **边界**：不读档案，不打开 blob，不看软触发和 64K 目标。

use crate::supersede::PROTECT_ROUNDS;
use crate::transcript::{ToolResult, View};
use serde_json::{json, Value};

/// 把保护窗口之外、正文仍是原文的工具结果换成陈旧存根。
///
/// **作用**：L2 的遮蔽函数。测试直接调用。
/// **参数**：`view` 只改命中的工具 `body`，空视图允许。`edits` 只在末尾追加命中的 edit，已有内容保留。
/// **边界**：最近 [`PROTECT_ROUNDS`] 个模型轮不动。正文已以 `[unchanged:`、`[superseded:` 或 `[stale:` 开头时跳过。
/// 不改 `system`、`user`、assistant 和 `trailing_user`。不读规格，不改策略名，不打开 blob。
/// 从最旧一轮的最早一次调用开始，窗口外的原文一批全部替换，不按目标中途停下。
/// 被 L0 指针指到的原文这一步仍会遮。
fn mask_stale(view: &mut View, edits: &mut Vec<Value>) {
    let protect_from = view.rounds.len().saturating_sub(PROTECT_ROUNDS);
    for round in 0..protect_from {
        let tool_count = view.rounds[round].tools.len();
        for call in 0..tool_count {
            if is_existing_stub(&view.rounds[round].tools[call].body) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{ModelUsage, TokenCount};

    use crate::transcript::{AssistantTurn, Round};

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
}
