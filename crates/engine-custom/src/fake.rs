use std::collections::VecDeque;

use async_trait::async_trait;
use fluxaxiom_contract::ModelUsage;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatCompletion, ChatError, FinishReason, ObservedText, ToolCall};
use crate::retry::{ChatTransport, Round};

/// Fake 按顺序交回的一步。
///
/// **作用**：脚本化正常响应、HTTP 状态和流中断。
/// **边界**：不访问网络。假设来自 ADR-020：只有请求带了 `tools`、且某条 assistant 没有 `reasoning_content` 键时才 400。此时不消耗脚本。空字符串不拒绝。这不是对真实 API 的证明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeStep {
    Turn(ScriptedTurn),
    Status(u16),
    Interrupt,
}

/// 一次成功响应的 delta。拼接后才是交给调用方的正文。
///
/// **作用**：模拟 SSE 里分开到达的 `reasoning_content`、`content` 和 `tool_calls`。
/// **边界**：`arguments` 不是 JSON 时，这一步变成非法工具参数，不重试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTurn {
    pub reasoning_deltas: Vec<String>,
    pub content_deltas: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: ModelUsage,
}

/// 脚本驱动的 Chat Completions 传输。
///
/// **作用**：让测试覆盖 400、429、5xx、流中断和非法工具参数。
/// **边界**：不读密钥，不写日志。脚本耗尽是测试编写错误。
#[derive(Debug, Clone)]
pub struct FakeChat {
    steps: VecDeque<FakeStep>,
}

impl FakeChat {
    /// 按给定顺序构造 Fake。
    ///
    /// **作用**：每个 `round` 取走一步，除非请求本身就会 400，或取消令牌已触发。
    /// **参数**：`steps` 按调用顺序消耗。请求因缺少 `reasoning_content` 被拒绝时不消耗。取消时也不消耗。
    /// **边界**：空脚本在第一次真正取步时 panic。
    #[must_use]
    pub fn script(steps: impl IntoIterator<Item = FakeStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
        }
    }
}

#[async_trait]
impl ChatTransport for FakeChat {
    /// **作用**：按脚本交回下一步，供重试测试使用。
    /// **参数**：`request_json` 用来判断带 `tools` 的请求是否漏了 `reasoning_content`。`cancel` 已触发时返回 `Cancelled`。
    /// **边界**：400 与取消都不消耗脚本，也不访问网络。空脚本在真正取步时 panic。
    async fn round(
        &mut self,
        request_json: &Value,
        cancel: &CancellationToken,
    ) -> Result<Round, ChatError> {
        if cancel.is_cancelled() {
            return Ok(Round::Cancelled);
        }
        if request_rejected(request_json) {
            return Ok(Round::Status {
                status: 400,
                summary: String::new(),
            });
        }
        let step = self
            .steps
            .pop_front()
            .expect("fake chat script has no step left");
        Ok(match step {
            FakeStep::Status(status) => Round::Status {
                status,
                summary: String::new(),
            },
            FakeStep::Interrupt => Round::Interrupted,
            FakeStep::Turn(turn) => assemble(turn),
        })
    }
}

/// 判断序列化后的请求会不会被 API 以 400 拒绝。
///
/// **参数**：`request_json` 是即将发送的请求。只有 `tools` 键存在，且某条 assistant 没有 `reasoning_content` 或该值是 `null`，才算拒绝。
/// **边界**：没有 `tools` 键时不因思维链拒绝。空字符串不算缺失。`tools: []` 仍视为携带了该键，但类型化请求不会发出空数组。
fn request_rejected(request_json: &Value) -> bool {
    if request_json.get("tools").is_none() {
        return false;
    }
    request_json
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| messages.iter().any(assistant_missing_reasoning))
}

/// 判断一条消息是不是省略了 `reasoning_content` 的 assistant。
///
/// **参数**：`message` 是请求 JSON 里的一条消息。
/// **边界**：其他角色不检查这个字段。`null` 和非字符串都算没有回传。
fn assistant_missing_reasoning(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
        && !matches!(message.get("reasoning_content"), Some(Value::String(_)))
}

/// 把脚本里的 delta 收成一次完成结果。
///
/// **参数**：`turn` 携带分段文本、工具调用和用量。
/// **边界**：先查畸形（空 id、空 name、id 重复），再查 `arguments` 是否为 JSON。任一失败不产生 `Completed`，不重试。
/// `finish_reason` 是 `Length` 时与 `parse_sse` 一致：不检查工具调用，`tool_calls` 一律为空，响应仍是 `Completed`。
fn assemble(mut turn: ScriptedTurn) -> Round {
    if turn.finish_reason == FinishReason::Length {
        turn.tool_calls.clear();
    }
    let mut seen_ids = std::collections::HashSet::new();
    for call in &turn.tool_calls {
        if call.id.is_empty() || call.name.is_empty() || !seen_ids.insert(call.id.as_str()) {
            return Round::MalformedToolCall {
                usage: turn.usage.clone(),
            };
        }
        if serde_json::from_str::<Value>(&call.arguments).is_err() {
            return Round::InvalidToolArguments {
                usage: turn.usage.clone(),
            };
        }
    }
    Round::Completed(ChatCompletion {
        reasoning: observed_from_deltas(&turn.reasoning_deltas),
        content: observed_from_deltas(&turn.content_deltas),
        tool_calls: turn.tool_calls,
        finish_reason: turn.finish_reason,
        usage: turn.usage,
    })
}

/// 把脚本 delta 收成观测值。
///
/// **参数**：`deltas` 是这一字段实际出现过的片段。空切片表示字段没出现。
/// **边界**：只含空字符串时是 `Empty`，不是 `Absent`。
fn observed_from_deltas(deltas: &[String]) -> ObservedText {
    if deltas.is_empty() {
        ObservedText::Absent
    } else {
        let text = deltas.concat();
        if text.is_empty() {
            ObservedText::Empty
        } else {
            ObservedText::Text(text)
        }
    }
}
