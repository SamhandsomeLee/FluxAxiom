use serde_json::Value;

use crate::chat::{observe_reasoning_text, ChatCompletion, ChatError, FinishReason, ObservedText};

/// 解析一段已经读完的 Chat Completions SSE 正文。
///
/// **作用**：把 `delta.reasoning_content` 和 `delta.content` 分成两个观测值，并取结束块里的 `finish_reason` 与 usage。
/// **参数**：`body` 是 HTTP 响应体。`data:` 行里的 JSON 字段名来自思考模式文档的 Python 样例；`data:` / `[DONE]` 分帧是 Chat Completions 的 SSE 信封，该页没有给出原始字节样例。
/// **边界**：没见到 `[DONE]` 且没有非空 `finish_reason` 时返回 `StreamInterrupted`，不返回已拼片段。`delta.tool_calls` 为非 null 时返回 `UnsupportedDelta`，这一刀不解析工具分片。JSON `null` 不当成工具分片。
/// `usage: null` 不覆盖已经拿到的用量对象。中间块在 `include_usage: true` 时就是 null，统计在最后一个块上。未知 `finish_reason` 放进 `FinishReason::Other`，不改写成 `stop`。
pub fn parse_sse(body: &str) -> Result<ChatCompletion, ChatError> {
    let mut reasoning = Accumulator::default();
    let mut content = Accumulator::default();
    let mut finish_reason = None;
    let mut usage = None;
    let mut saw_done = false;
    for line in body.split('\n') {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            saw_done = true;
            break;
        }
        let chunk: Value = serde_json::from_str(data).map_err(|_| ChatError::StreamInterrupted)?;
        let choice = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(choice) = choice {
            if let Some(delta) = choice.get("delta") {
                if delta
                    .get("tool_calls")
                    .is_some_and(|value| !value.is_null())
                {
                    return Err(ChatError::UnsupportedDelta);
                }
                push_field(&mut reasoning, delta.get("reasoning_content"));
                push_field(&mut content, delta.get("content"));
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                finish_reason = Some(parse_finish_reason(reason));
            }
        }
        if let Some(object) = chunk.get("usage").filter(|value| value.is_object()) {
            usage = Some(object.clone());
        }
    }
    let Some(finish_reason) = finish_reason else {
        return Err(ChatError::StreamInterrupted);
    };
    if !saw_done {
        return Err(ChatError::StreamInterrupted);
    }
    Ok(ChatCompletion {
        reasoning: reasoning.finish(),
        content: content.finish(),
        tool_calls: Vec::new(),
        finish_reason,
        usage: usage
            .as_ref()
            .map(crate::chat::usage_from_json)
            .unwrap_or_else(|| crate::chat::usage_from_json(&Value::Null)),
    })
}

#[derive(Default)]
struct Accumulator {
    seen: bool,
    text: String,
}

impl Accumulator {
    /// **作用**：把拼接结果收成 `Absent`、`Empty` 或 `Text`。
    /// **边界**：一个 delta 都没见过是 `Absent`。见过但拼出来是空文本才是 `Empty`。
    fn finish(self) -> ObservedText {
        if !self.seen {
            ObservedText::Absent
        } else {
            observe_reasoning_text(&self.text)
        }
    }
}

/// 只在 JSON 里真有这个键时追加。
///
/// **作用**：让后面能区分「没出现」和「出现过的空串」。
/// **参数**：`field` 累积这一字段的文本。`value` 为 `None` 表示键不存在。
/// **边界**：只有字符串才会拼接。`null` 不把字段标成已出现，也不把缺失编成空串。
fn push_field(field: &mut Accumulator, value: Option<&Value>) {
    let Some(Value::String(text)) = value else {
        return;
    };
    field.seen = true;
    field.text.push_str(text);
}

/// 把结束原因字符串收成枚举。
///
/// **参数**：`reason` 是 SSE 块里的 `finish_reason`。
/// **边界**：`stop`、`length`、`tool_calls` 以外放进 `Other`，保留原文，不改写成 `stop`。空字符串由调用方滤掉。
fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Other(reason.to_owned()),
    }
}
