use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use fluxaxiom_contract::ModelUsage;

use crate::chat::{
    observe_reasoning_text, usage_from_json, ChatCompletion, ChatError, FinishReason, ObservedText,
    ToolCall,
};

/// 流式拼接过程中记下的工具错误，用量在收尾时再挂上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingToolError {
    Malformed,
    InvalidArguments,
}

/// 解析一段已经读完的 Chat Completions SSE 正文。
///
/// **作用**：把 `delta.reasoning_content`、`delta.content` 和 `delta.tool_calls` 收成完整响应，并取结束块里的 `finish_reason` 与 usage。
/// **参数**：`body` 是 HTTP 响应体。`data:` 行里的 JSON 字段名来自思考模式文档的 Python 样例；`data:` / `[DONE]` 分帧是 Chat Completions 的 SSE 信封。流式 `tool_calls` 按 create-chat-completion 的 chunk schema 假设拼接，样本是手写的，待真实响应替换。
/// **边界**：没见到 `[DONE]` 且没有非空 `finish_reason` 时返回 `StreamInterrupted`，不返回已拼片段。JSON `null` 的 `tool_calls` 不当成工具分片。未知其它 delta 字段忽略。工具分片畸形是 `MalformedToolCall`；`arguments` 拼完不是 JSON 是 `InvalidToolArguments`。`usage: null` 不覆盖已经拿到的用量对象。
/// **`finish_reason = length` 优先于工具分片的错误**：输出触顶时工具调用可能在任何位置被截断（参数不完整、`id` 还没到），这种截断不是服务端违约，而是输出额度耗尽。
/// 所以 `length` 时不报 `MalformedToolCall` 或 `InvalidToolArguments`，而是返回 `Length` 的完成结果，`tool_calls` 一律为空，不把可能被截断的调用交给调用方。Loop 凭 `Length` 决定终态，见执行方案 §4.7.5。
/// 非 `length` 的畸形分片仍是错误，即使流已经读到结尾；错误在读完整个流、确认了 `finish_reason` 之后才返回。
pub fn parse_sse(body: &str) -> Result<ChatCompletion, ChatError> {
    let mut reasoning = Accumulator::default();
    let mut content = Accumulator::default();
    let mut tool_calls = BTreeMap::<u64, ToolCallBuilder>::new();
    // 第一处分片错误。先记下，读到 `finish_reason` 后再决定是否报告，见上面的 `length` 说明。
    let mut tool_error: Option<PendingToolError> = None;
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
                push_field(&mut reasoning, delta.get("reasoning_content"));
                push_field(&mut content, delta.get("content"));
                if tool_error.is_none() {
                    if let Err(error) =
                        apply_tool_call_deltas(&mut tool_calls, delta.get("tool_calls"))
                    {
                        tool_error = Some(error);
                    }
                }
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
    let resolved_usage = usage
        .as_ref()
        .map(usage_from_json)
        .unwrap_or_else(|| usage_from_json(&Value::Null));
    let tool_calls = if finish_reason == FinishReason::Length {
        Vec::new()
    } else if let Some(error) = tool_error {
        return Err(pending_to_chat_error(error, resolved_usage));
    } else {
        match finish_tool_calls(tool_calls) {
            Ok(calls) => calls,
            Err(error) => return Err(pending_to_chat_error(error, resolved_usage)),
        }
    };
    Ok(ChatCompletion {
        reasoning: reasoning.finish(),
        content: content.finish(),
        tool_calls,
        finish_reason,
        usage: resolved_usage,
    })
}

/// 把拼接期错误挂上本次用量，供 Loop 在失败前 observe。
///
/// **参数**：`error` 是畸形或非法 arguments。`usage` 来自流末块（缺失则为全部 Unknown）。
fn pending_to_chat_error(error: PendingToolError, usage: ModelUsage) -> ChatError {
    match error {
        PendingToolError::Malformed => ChatError::MalformedToolCall { usage },
        PendingToolError::InvalidArguments => ChatError::InvalidToolArguments { usage },
    }
}

/// 流式工具调用按 `index` 累积的中间状态。
///
/// **作用**：同一 index 的分片拼成一条 `ToolCall`。
/// **边界**：`id` 与 `name` 只接受第一个非空值；之后再出现不同的非空值是畸形。
#[derive(Default)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// 把一块 `delta.tool_calls` 并进累积表。
///
/// **参数**：`tool_calls` 是按 index 排序的累积表。`value` 为 `None` 或 JSON `null` 时什么都不做。
/// **边界**：必须是数组。每个元素必须有整数 `index`。`type` 若出现必须是 `function`。同 index 上非空 `id`/`name` 冲突是 `MalformedToolCall`。
fn apply_tool_call_deltas(
    tool_calls: &mut BTreeMap<u64, ToolCallBuilder>,
    value: Option<&Value>,
) -> Result<(), PendingToolError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    let Some(items) = value.as_array() else {
        return Err(PendingToolError::Malformed);
    };
    for item in items {
        let Some(index) = item.get("index").and_then(Value::as_u64) else {
            return Err(PendingToolError::Malformed);
        };
        let entry = tool_calls.entry(index).or_default();
        if let Some(kind) = item.get("type").and_then(Value::as_str) {
            if kind != "function" {
                return Err(PendingToolError::Malformed);
            }
        }
        if let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            match &entry.id {
                None => entry.id = Some(id.to_owned()),
                Some(existing) if existing == id => {}
                Some(_) => return Err(PendingToolError::Malformed),
            }
        }
        if let Some(function) = item.get("function") {
            if let Some(name) = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            {
                match &entry.name {
                    None => entry.name = Some(name.to_owned()),
                    Some(existing) if existing == name => {}
                    Some(_) => return Err(PendingToolError::Malformed),
                }
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(arguments);
            }
        }
    }
    Ok(())
}

/// 把累积表收成按 index 排序的 `ToolCall` 列表。
///
/// **参数**：`tool_calls` 是流里见过的全部 index。
/// **边界**：先查畸形（缺 id、空 name、id 重复），再查 `arguments` 是否为 JSON。空串 `""` 不是合法 JSON，是 `InvalidToolArguments`。
fn finish_tool_calls(
    tool_calls: BTreeMap<u64, ToolCallBuilder>,
) -> Result<Vec<ToolCall>, PendingToolError> {
    let mut seen_ids = HashSet::new();
    let mut finished = Vec::with_capacity(tool_calls.len());
    for (_, entry) in tool_calls {
        let Some(id) = entry.id.filter(|id| !id.is_empty()) else {
            return Err(PendingToolError::Malformed);
        };
        let Some(name) = entry.name.filter(|name| !name.is_empty()) else {
            return Err(PendingToolError::Malformed);
        };
        if !seen_ids.insert(id.clone()) {
            return Err(PendingToolError::Malformed);
        }
        if serde_json::from_str::<Value>(&entry.arguments).is_err() {
            return Err(PendingToolError::InvalidArguments);
        }
        finished.push(ToolCall {
            id,
            name,
            arguments: entry.arguments,
        });
    }
    Ok(finished)
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
