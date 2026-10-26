//! 可重建性校验（A4b / §4.12.1）。
//!
//! **作用**：检查第 k 次请求的消息历史是否等于前一轮请求 + assistant(resp_k) + tool 结果；
//! 以及跨轮 `model` / `tools` / system 一致性、尾轮规则、事件与响应字段一致。
//! **边界**：纯函数，输入已是 JSON / 字节；不依赖 `run-host`。读不到 blob 时为 `unverifiable`，不猜成一致。

use fluxaxiom_contract::{ModelUsage, RebuildResult, TokenCount};
use serde_json::Value;

/// 一轮模型调用的已读出材料。
///
/// **作用**：CLI / 测试把 `RunTrace` 轮次与 blob 填进这里再调用 [`verify`]。
/// **边界**：`request` / `response` 为 `None` 表示对应 blob 读不到或不是 JSON；工具正文缺失同理。
/// 派生的 `Debug` 会打印这些 JSON，其中可含 prompt 与 `reasoning_content`，不要写入普通日志。
#[derive(Debug, Clone)]
pub struct RebuildCall {
    /// 从 1 起的模型轮次序号（与 `flux trace` 一致）。
    pub index: u32,
    /// 请求 blob 解析结果；缺省为 unverifiable。
    pub request: Option<Value>,
    /// 响应 blob 解析结果。
    pub response: Option<Value>,
    /// 该轮工具事件的调用身份与正文，保持事件顺序。
    pub tool_results: Vec<RebuildTool>,
    /// 事件里的 `finish_reason`。
    pub event_finish_reason: String,
    /// 事件里的 `usage`。
    pub event_usage: ModelUsage,
}

/// 已读出的工具事件。身份用于核对前缀，正文用于核对下一轮 tool 消息。
///
/// **边界**：旧事件没有调用 ID 时为 None，不能推断为已验证；正文只用于内存比较。
#[derive(Clone)]
pub struct RebuildTool {
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub result: Option<Vec<u8>>,
}

impl std::fmt::Debug for RebuildTool {
    /// **作用**：展示身份和字节数，不把工具正文写入调试日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RebuildTool")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("bytes", &self.result.as_ref().map(Vec::len))
            .finish()
    }
}

/// 对一整次 Run 的模型轮次做可重建性校验。
///
/// **作用**：返回三态 [`RebuildResult`]；`inconsistent` 带第一处不符的调用序号与规则号。
/// **参数**：`calls` 按模型轮次升序；空切片视为 `consistent`（无模型调用可核对）。
/// **边界**：规则见执行方案 §4.12.1。不重放 Engine，不拨号。
#[must_use]
pub fn verify(calls: &[RebuildCall]) -> RebuildResult {
    if calls.is_empty() {
        return RebuildResult::Consistent;
    }
    for call in calls {
        if !call.request.as_ref().is_some_and(valid_request) {
            return RebuildResult::Unverifiable {
                missing: format!("request_ref#{}", call.index),
            };
        }
        if !call.response.as_ref().is_some_and(valid_response) {
            return RebuildResult::Unverifiable {
                missing: format!("response_ref#{}", call.index),
            };
        }
        for (j, tool) in call.tool_results.iter().enumerate() {
            if tool.result.is_none() {
                return RebuildResult::Unverifiable {
                    missing: format!("tool_result#{}:{}", call.index, j + 1),
                };
            }
            if tool.tool_call_id.as_ref().is_none_or(|id| id.is_empty()) {
                return RebuildResult::Unverifiable {
                    missing: format!("tool_call_id#{}:{}", call.index, j + 1),
                };
            }
        }
    }

    let first = calls[0].request.as_ref().expect("checked");
    let first_model = first.get("model").cloned();
    let first_effort = first.get("reasoning_effort").cloned();
    let first_max_tokens = first.get("max_tokens").cloned();
    let first_tools = first.get("tools").cloned();
    let first_system = first_system_message(first);

    let n = calls.len();
    for (pos, call) in calls.iter().enumerate() {
        let k = call.index;
        let req = call.request.as_ref().expect("checked");
        let resp = call.response.as_ref().expect("checked");
        let is_last = pos + 1 == n;

        // 规则 5：跨轮字段与首条 system 相同。
        if req.get("model") != first_model.as_ref()
            || req.get("reasoning_effort") != first_effort.as_ref()
            || req.get("max_tokens") != first_max_tokens.as_ref()
            || req.get("tools") != first_tools.as_ref()
            || first_system_message(req) != first_system
        {
            return inconsistent(k, 5, "request model/tools/system differ across calls");
        }

        // 规则 7：事件 finish_reason / usage 与响应一致。
        let resp_finish = resp
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        if resp_finish != call.event_finish_reason {
            return inconsistent(
                k,
                7,
                format!(
                    "finish_reason event={} response={}",
                    call.event_finish_reason, resp_finish
                ),
            );
        }
        if let Err(detail) = usage_matches(resp.get("usage"), &call.event_usage) {
            return inconsistent(k, 7, detail);
        }

        let req_has_tools = req.get("tools").is_some();
        let resp_tool_calls = resp_tool_calls(resp).expect("validated response");
        let tool_event_count = call.tool_results.len();

        if is_last {
            // 规则 6：尾轮。
            if call.event_finish_reason == "tool_calls" {
                if tool_event_count > resp_tool_calls.len() {
                    return inconsistent(k, 6, "last call tool events exceed response tool_calls");
                }
                if let Err(detail) = tools_are_prefix(&resp_tool_calls, &call.tool_results) {
                    return inconsistent(k, 6, detail);
                }
            } else if tool_event_count != 0 {
                return inconsistent(k, 6, "non-tool_calls finish must have no tool events");
            }
        } else {
            // 规则 4：非尾轮工具事件个数等于 tool_calls。
            if tool_event_count != resp_tool_calls.len() {
                return inconsistent(
                    k,
                    4,
                    format!(
                        "tool events {} != response tool_calls {}",
                        tool_event_count,
                        resp_tool_calls.len()
                    ),
                );
            }
            if let Err(detail) = tools_are_prefix(&resp_tool_calls, &call.tool_results) {
                return inconsistent(k, 4, detail);
            }
            let next = calls[pos + 1].request.as_ref().expect("checked");
            if let Err((rule, detail)) =
                check_history_extension(req, next, resp, &call.tool_results, req_has_tools)
            {
                return inconsistent(k, rule, detail);
            }
        }
    }
    RebuildResult::Consistent
}

/// 把三态收成 CLI 一行文本。
///
/// **作用**：`flux trace --verify` 在文本输出末尾追加这一行。
/// **参数**：`result` 是 [`verify`] 的返回值。
/// **边界**：只拼 `rebuild=consistent` / `inconsistent` / `unverifiable`。`detail` 原样附上，不截断、不读 blob。
/// `--json` 由调用方序列化 [`RebuildResult`]，不经过这里。
#[must_use]
pub fn format_rebuild_line(result: &RebuildResult) -> String {
    match result {
        RebuildResult::Consistent => "rebuild=consistent".to_owned(),
        RebuildResult::Inconsistent { call, rule, detail } => {
            format!("rebuild=inconsistent call=#{call} rule={rule} {detail}")
        }
        RebuildResult::Unverifiable { missing } => {
            format!("rebuild=unverifiable missing={missing}")
        }
    }
}

/// 构造第一处不符，供 `verify` 立刻返回。
///
/// **作用**：把规则号和短原因收成 [`RebuildResult::Inconsistent`]。
/// **参数**：`call` 是 [`RebuildCall::index`]（从 1 起）；`rule` 是执行方案 §4.12.1 的规则号；`detail` 给 CLI 看，不应含请求或工具正文。
/// **边界**：不表示 blob 缺失。调用后不再检查后面的轮次。
fn inconsistent(call: u32, rule: u8, detail: impl Into<String>) -> RebuildResult {
    RebuildResult::Inconsistent {
        call,
        rule,
        detail: detail.into(),
    }
}

/// 取请求 `messages` 的第一条，且 `role` 必须是 `system`。
///
/// **作用**：规则 5 用它比较各轮首条 system 是否为同一个 JSON 值。
/// **参数**：`req` 是已解析的 Chat 请求。
/// **边界**：缺 `messages`、空数组、或首条不是 system 时返回 `None`。不检查其后的消息。
fn first_system_message(req: &Value) -> Option<&Value> {
    req.get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.first())
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
}

/// 核对下一轮请求是否只在上一轮消息之后追加了 assistant 和 tool。
///
/// **作用**：非尾轮由 [`verify`] 调用。`Err` 的第一个分量是规则号。
/// **参数**：`prev` / `next` 是相邻请求；`resp` 是上一轮响应；工具正文已经读到，顺序已经核对。
/// **边界**：`messages` 缺失按规则 5 返回。历史前缀被改、后缀条数不对、assistant 的 `content` / `reasoning_content` 不符按规则 1；`tool_calls` 的 id、name、arguments 或顺序不符按规则 2；tool 的 id 或正文字节不符按规则 3。没有下一轮时不要调用。
fn check_history_extension(
    prev: &Value,
    next: &Value,
    resp: &Value,
    tool_results: &[RebuildTool],
    req_has_tools: bool,
) -> Result<(), (u8, String)> {
    let prev_messages = messages_array(prev).ok_or((5, "prev messages missing".into()))?;
    let next_messages = messages_array(next).ok_or((5, "next messages missing".into()))?;
    if next_messages.len() < prev_messages.len() {
        return Err((1, "next messages shorter than prev".into()));
    }
    if next_messages[..prev_messages.len()] != *prev_messages {
        return Err((1, "next messages do not start with prev messages".into()));
    }
    let suffix = &next_messages[prev_messages.len()..];
    let expected_tools = tool_results.len();
    if suffix.len() != 1 + expected_tools {
        return Err((
            1,
            format!(
                "suffix len {} != 1+{} tool messages",
                suffix.len(),
                expected_tools
            ),
        ));
    }
    let assistant = &suffix[0];
    check_assistant(assistant, resp, req_has_tools)?;
    let resp_calls = resp_tool_calls(resp).expect("validated response");
    for (j, tool_msg) in suffix[1..].iter().enumerate() {
        check_tool_message(
            tool_msg,
            &resp_calls,
            j,
            tool_results[j].result.as_ref().unwrap(),
        )?;
    }
    Ok(())
}

/// 核对追加的 assistant 消息是否等于该轮响应。
///
/// **作用**：规则 1 比较 `content` 与 `reasoning_content`；规则 2 比较 `tool_calls`。
/// **参数**：`assistant` 是下一轮请求里新追加的第一条；`resp` 是上一轮响应；`req_has_tools` 为真时要求 `reasoning_content` 键存在，空串合法。
/// **边界**：`content` 缺省或 `null` 当成空串。响应里 `reasoning_content` 为 `null`、请求里是 `""` 视为相同。不带 `tools` 的请求不要求 `reasoning_content` 键。
fn check_assistant(
    assistant: &Value,
    resp: &Value,
    req_has_tools: bool,
) -> Result<(), (u8, String)> {
    if assistant.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err((1, "expected assistant role".into()));
    }
    let content = text_field(assistant.get("content"));
    let resp_content = text_field(resp.get("content"));
    if content != resp_content {
        return Err((1, "assistant content != response content".into()));
    }
    if req_has_tools {
        match assistant.get("reasoning_content") {
            Some(Value::String(text)) => {
                let resp_reasoning = text_field(resp.get("reasoning_content"));
                if text != &resp_reasoning {
                    // Host 在 Absent 时补 `""`；响应 blob 可能是 null。
                    if !(text.is_empty() && resp.get("reasoning_content") == Some(&Value::Null)) {
                        return Err((1, "assistant reasoning_content != response".into()));
                    }
                }
            }
            _ => return Err((1, "assistant missing reasoning_content key".into())),
        }
    }
    let assistant_calls =
        assistant_tool_calls(assistant).ok_or((2, "invalid assistant.tool_calls".into()))?;
    let resp_calls = resp_tool_calls(resp).expect("validated response");
    if assistant_calls != resp_calls {
        return Err((2, "assistant.tool_calls != response.tool_calls".into()));
    }
    Ok(())
}

/// 核对一条 tool 消息的 `tool_call_id` 与正文。
///
/// **作用**：规则 3。由 [`check_history_extension`] 按该轮工具顺序调用。
/// **参数**：`j` 是该轮内从 0 起的序号；`bytes` 是对应 `result_ref` blob 的原始字节；`resp_calls` 是响应里已摊平的 `tool_calls`。
/// **边界**：`content` 按 UTF-8 字符串的字节与 blob 比较。id 不符、字节不符、或响应里没有第 `j` 个 tool_call，都返回规则 3。
fn check_tool_message(
    tool_msg: &Value,
    resp_calls: &[FlatToolCall],
    j: usize,
    bytes: &[u8],
) -> Result<(), (u8, String)> {
    if tool_msg.get("role").and_then(Value::as_str) != Some("tool") {
        return Err((3, "expected tool role".into()));
    }
    let Some(expected) = resp_calls.get(j) else {
        return Err((3, format!("no response tool_call at index {j}")));
    };
    let id = tool_msg
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if id != expected.id {
        return Err((3, format!("tool_call_id {id} != {}", expected.id)));
    }
    let content = tool_msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if content.as_bytes() != bytes {
        return Err((3, "tool content bytes != result_ref blob".into()));
    }
    Ok(())
}

/// 工具事件的调用 ID 与工具名必须逐项等于响应的前缀。
///
/// **作用**：非尾轮用于规则 4，尾轮用于规则 6；不执行工具，不读取当前工作区。
/// **边界**：调用 ID 缺失已在 verify 返回 unverifiable；错误信息不带参数或正文。
fn tools_are_prefix(
    resp_calls: &[FlatToolCall],
    tool_results: &[RebuildTool],
) -> Result<(), String> {
    if tool_results.len() > resp_calls.len() {
        return Err("tool events longer than tool_calls".into());
    }
    for (j, (event, call)) in tool_results.iter().zip(resp_calls).enumerate() {
        if event.tool_call_id.as_deref() != Some(call.id.as_str()) || event.tool_name != call.name {
            return Err(format!(
                "tool event {} does not match response prefix",
                j + 1
            ));
        }
    }
    Ok(())
}

/// 取出请求的 `messages` 数组。
///
/// **作用**：历史追加检查的两侧输入。
/// **边界**：缺字段或不是数组时 `None`。
fn messages_array(req: &Value) -> Option<&[Value]> {
    req.get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
}

/// 把 JSON 文本字段收成字符串，供 `content` / `reasoning_content` 比较。
///
/// **参数**：`value` 是字段本身；调用方已经做过 `get`。
/// **边界**：缺省和 `null` 都是空串。其它 JSON 类型用其 `Display`，不当成空串。
fn text_field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 摊平后的一次 tool_call，只留 `id`、`name`、`arguments`。
///
/// **作用**：让响应的平铺形状和请求里的 OpenAI `function` 嵌套形状可以按值比较。
/// **边界**：派生的 `Debug` 会打印 `arguments`。不要把本类型写进普通日志。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// 从响应 blob 读取平铺的 `tool_calls`。
///
/// **作用**：规则 2、4、6 用的响应侧列表。
/// **参数**：`resp` 是该轮响应 JSON。
/// **边界**：任一非法条目使整个解析失败。usage-bearing 失败响应允许省略列表。
fn resp_tool_calls(resp: &Value) -> Option<Vec<FlatToolCall>> {
    match resp.get("tool_calls") {
        None => Some(Vec::new()),
        Some(value) => value.as_array()?.iter().map(flat_tool_call).collect(),
    }
}

/// 从请求里的 assistant 消息读取 `tool_calls`。
///
/// **作用**：规则 2，与 [`resp_tool_calls`] 的结果按顺序比较。
/// **参数**：`assistant` 是一条 `role=assistant` 的消息。
/// **边界**：只接受请求的 function 嵌套形状，不能用平铺字段掩盖损坏的嵌套字段。
fn assistant_tool_calls(assistant: &Value) -> Option<Vec<FlatToolCall>> {
    match assistant.get("tool_calls") {
        None => Some(Vec::new()),
        Some(value) => value
            .as_array()?
            .iter()
            .map(|item| {
                if item.get("type")?.as_str()? != "function" {
                    return None;
                }
                let function = item.get("function")?;
                let arguments = function.get("arguments")?.as_str()?;
                serde_json::from_str::<Value>(arguments).ok()?;
                Some(FlatToolCall {
                    id: nonempty_string(item.get("id"))?.to_owned(),
                    name: nonempty_string(function.get("name"))?.to_owned(),
                    arguments: arguments.to_owned(),
                })
            })
            .collect(),
    }
}

/// 解析一个响应侧工具调用；任一必需字段非法时失败，不过滤条目。
fn flat_tool_call(item: &Value) -> Option<FlatToolCall> {
    let arguments = item.get("arguments")?.as_str()?;
    serde_json::from_str::<Value>(arguments).ok()?;
    Some(FlatToolCall {
        id: nonempty_string(item.get("id"))?.to_owned(),
        name: nonempty_string(item.get("name"))?.to_owned(),
        arguments: arguments.to_owned(),
    })
}

/// 读取非空字符串。只用于结构校验，不输出字段内容。
fn nonempty_string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|text| !text.is_empty())
}

/// 校验完整请求的基本结构；跨轮相等和 reasoning 必需键仍由规则 1–5 判断。
///
/// **边界**：max_tokens 和 tools 可以省略；存在时必须符合本 crate 的请求形状。
fn valid_request(req: &Value) -> bool {
    if !req.is_object()
        || nonempty_string(req.get("model")).is_none()
        || !matches!(
            req.get("reasoning_effort").and_then(Value::as_str),
            Some("low" | "high" | "max")
        )
        || req.get("max_tokens").is_some_and(|v| {
            v.as_u64()
                .is_none_or(|n| !(1..=u64::from(crate::MAX_TOKENS_LIMIT)).contains(&n))
        })
    {
        return false;
    }
    if let Some(tools) = req.get("tools") {
        let Some(tools) = tools.as_array().filter(|tools| !tools.is_empty()) else {
            return false;
        };
        if !tools.iter().all(|tool| {
            tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("function").is_some_and(|function| {
                    nonempty_string(function.get("name")).is_some()
                        && function.get("description").is_some_and(Value::is_string)
                        && function.get("parameters").is_some_and(Value::is_object)
                })
        }) {
            return false;
        }
    }
    let Some(messages) = messages_array(req) else {
        return false;
    };
    first_system_message(req).is_some()
        && messages.iter().all(
            |message| match message.get("role").and_then(Value::as_str) {
                Some("system" | "user") => message.get("content").is_some_and(Value::is_string),
                Some("assistant") => {
                    optional_text(message.get("content"))
                        && optional_text(message.get("reasoning_content"))
                        && assistant_tool_calls(message).is_some()
                }
                Some("tool") => {
                    message.get("content").is_some_and(Value::is_string)
                        && nonempty_string(message.get("tool_call_id")).is_some()
                }
                _ => false,
            },
        )
}

/// 响应结构校验，兼容只含 finish_reason / usage 的已记录失败响应。
///
/// **边界**：只有两个已知的本地失败标记允许省略正文；其它响应缺字段或有非法条目均不可验证。
fn valid_response(resp: &Value) -> bool {
    let Some(finish) = nonempty_string(resp.get("finish_reason")) else {
        return false;
    };
    let usage_valid = resp
        .get("usage")
        .is_some_and(|value| serde_json::from_value::<ModelUsage>(value.clone()).is_ok());
    let usage_only_failure = matches!(finish, "malformed_tool_call" | "invalid_tool_arguments");
    usage_valid
        && optional_text(resp.get("content"))
        && optional_text(resp.get("reasoning_content"))
        && (usage_only_failure
            || ["content", "reasoning_content", "tool_calls"]
                .iter()
                .all(|key| resp.get(key).is_some()))
        && resp_tool_calls(resp).is_some()
}

/// Absent / null 文本与空文本的兼容边界；数值、对象和数组一律拒绝。
fn optional_text(value: Option<&Value>) -> bool {
    value.is_none_or(|v| v.is_null() || v.is_string())
}

/// 规则 7：响应 `usage` 的四个计数与事件里的 [`ModelUsage`] 相同。
///
/// **参数**：`resp_usage` 是响应 JSON 的 `usage` 字段；`event` 来自 `model.call_completed`。
/// **边界**：缺 `usage` 对象即失败。不比较 `prompt_layout_version` 或其它字段。
fn usage_matches(resp_usage: Option<&Value>, event: &ModelUsage) -> Result<(), String> {
    let Some(obj) = resp_usage else {
        return Err("response usage missing".into());
    };
    check_token(obj, "prompt_tokens", event.prompt_tokens)?;
    check_token(obj, "completion_tokens", event.completion_tokens)?;
    check_token(
        obj,
        "prompt_cache_hit_tokens",
        event.prompt_cache_hit_tokens,
    )?;
    check_token(
        obj,
        "prompt_cache_miss_tokens",
        event.prompt_cache_miss_tokens,
    )?;
    Ok(())
}

/// 比较 `usage` 对象里的一个计数。
///
/// **参数**：`key` 是响应里的字段名；`expected` 来自事件，可以是 `Known` 或 `Unknown`。
/// **边界**：JSON 数字收成 `Known`，字符串 `"unknown"` 收成 `Unknown`。缺字段、其它类型、或与事件不等都失败。不把缺失写成 0。
fn check_token(obj: &Value, key: &str, expected: TokenCount) -> Result<(), String> {
    let actual = match obj.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .map(TokenCount::Known)
            .ok_or_else(|| format!("{key} not u64"))?,
        Some(Value::String(s)) if s == "unknown" => TokenCount::Unknown,
        _ => return Err(format!("{key} missing or invalid in response")),
    };
    if actual != expected {
        return Err(format!("{key} event != response"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 各轮事件共用的已知用量。四个计数都是 `Known`，避免合计被 `unknown` 干扰。
    fn usage_known() -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(10),
            completion_tokens: TokenCount::Known(5),
            prompt_cache_hit_tokens: TokenCount::Known(0),
            prompt_cache_miss_tokens: TokenCount::Known(10),
        }
    }

    /// 与 [`usage_known`] 数值相同的响应 `usage` 对象。
    fn usage_json() -> Value {
        json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 10,
        })
    }

    /// 各轮共用的请求外壳：同一模型、`reasoning_effort`、`max_tokens` 和 `tools`。
    ///
    /// **参数**：`messages` 整段替换 `messages`，用来构造历史追加或故意残缺的反例。
    fn base_req(messages: Vec<Value>) -> Value {
        json!({
            "model": "deepseek-flash",
            "reasoning_effort": "high",
            "max_tokens": 32768,
            "tools": [{"type":"function","function":{"name":"list_files","description":"","parameters":{}}}],
            "messages": messages,
        })
    }

    /// 首轮固定的 system + user。规则 5 要求后续请求仍以这两条开头。
    fn system_user() -> Vec<Value> {
        vec![
            json!({"role":"system","content":"sys"}),
            json!({"role":"user","content":"task"}),
        ]
    }

    /// 一轮带单个 `list_files` 的响应。`finish_reason` 固定为 `tool_calls`。
    ///
    /// **参数**：`id` 写入唯一的 tool_call；`content` / `reasoning` 必须被下一轮 assistant 原样带回。
    fn resp_with_tools(id: &str, content: &str, reasoning: &str) -> Value {
        json!({
            "content": content,
            "reasoning_content": reasoning,
            "tool_calls": [{"id": id, "name": "list_files", "arguments": "{}"}],
            "finish_reason": "tool_calls",
            "usage": usage_json(),
            "prompt_layout_version": "1",
        })
    }

    /// 没有工具调用的收尾响应。`finish_reason` 固定为 `stop`，`tool_calls` 为空数组。
    fn resp_stop(content: &str, reasoning: &str) -> Value {
        json!({
            "content": content,
            "reasoning_content": reasoning,
            "tool_calls": [],
            "finish_reason": "stop",
            "usage": usage_json(),
            "prompt_layout_version": "1",
        })
    }

    /// 下一轮请求里应追加的 assistant，使用 OpenAI 嵌套 `function` 形状。
    ///
    /// **参数**：三个字符串必须与上一轮响应的 `content`、`reasoning_content`、tool_call `id` 一致。
    fn assistant_msg(content: &str, reasoning: &str, id: &str) -> Value {
        json!({
            "role": "assistant",
            "content": content,
            "reasoning_content": reasoning,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": "list_files", "arguments": "{}"}
            }]
        })
    }

    /// 一条 tool 消息。`body` 的 UTF-8 字节须等于该轮 `result_ref` blob。
    fn tool_msg(id: &str, body: &str) -> Value {
        json!({"role":"tool","tool_call_id": id, "content": body})
    }

    /// 组装一轮可校验材料。请求、响应和工具 blob 都视为已读到。
    ///
    /// **参数**：`tools` 的顺序就是该轮 `tool.call_completed` 的顺序；`finish` 同时写入事件的 `finish_reason`。用量用 [`usage_known`]。
    fn call(
        index: u32,
        request: Value,
        response: Value,
        tools: Vec<&[u8]>,
        finish: &str,
    ) -> RebuildCall {
        let response_calls = resp_tool_calls(&response).expect("valid test response");
        let tool_results = tools
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| RebuildTool {
                tool_call_id: Some(response_calls[index].id.clone()),
                tool_name: response_calls[index].name.clone(),
                result: Some(bytes.to_vec()),
            })
            .collect();
        RebuildCall {
            index,
            request: Some(request),
            response: Some(response),
            tool_results,
            event_finish_reason: finish.to_owned(),
            event_usage: usage_known(),
        }
    }

    /// 两轮工具后 `stop` 时应为 `consistent`。
    ///
    /// **作用**：锁住规则 1–5、7 的正向路径：第二轮消息是第一轮消息加上 assistant 和 tool。
    #[test]
    fn consistent_two_tool_rounds_then_stop() {
        let tool_body = b"files";
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(assistant_msg("", "r1", "c1"));
        msgs2.push(tool_msg("c1", "files"));
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        let result = verify(&[
            call(1, req1, resp1, vec![tool_body], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]);
        assert_eq!(result, RebuildResult::Consistent);
    }

    /// 下一轮丢掉 tool 消息时，第一轮按规则 1 失败。
    ///
    /// **边界**：后缀长度不是 `1 + 工具数`，不误报成规则 3。
    #[test]
    fn drop_tool_message_is_rule_1() {
        let tool_body = b"files";
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(assistant_msg("", "r1", "c1"));
        // 故意丢掉 tool 消息
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![tool_body], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { call, rule, .. } => {
                assert_eq!(call, 1);
                assert_eq!(rule, 1);
            }
            other => panic!("{other:?}"),
        }
    }

    /// 两条 tool 消息对调 id 时按规则 3 失败。
    ///
    /// **边界**：条数仍正确，所以不能落成规则 1 的后缀长度错误。
    #[test]
    fn swap_tool_messages_is_rule_3() {
        let req1 = base_req(system_user());
        let resp1 = json!({
            "content": "",
            "reasoning_content": "r",
            "tool_calls": [
                {"id":"a","name":"list_files","arguments":"{}"},
                {"id":"b","name":"list_files","arguments":"{}"}
            ],
            "finish_reason": "tool_calls",
            "usage": usage_json(),
        });
        let mut msgs2 = system_user();
        msgs2.push(json!({
            "role":"assistant","content":"","reasoning_content":"r",
            "tool_calls":[
                {"id":"a","type":"function","function":{"name":"list_files","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"list_files","arguments":"{}"}}
            ]
        }));
        // 交换两条 tool
        msgs2.push(tool_msg("b", "B"));
        msgs2.push(tool_msg("a", "A"));
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![b"A", b"B"], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 3),
            other => panic!("{other:?}"),
        }
    }

    /// tool 正文与 `result_ref` 差一个字节时按规则 3 失败。
    #[test]
    fn change_tool_byte_is_rule_3() {
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(assistant_msg("", "r1", "c1"));
        msgs2.push(tool_msg("c1", "filesX"));
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![b"files"], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 3),
            other => panic!("{other:?}"),
        }
    }

    /// `tool_call_id` 与响应 tool_call 不一致时按规则 3 失败。
    #[test]
    fn wrong_tool_call_id_is_rule_3() {
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(assistant_msg("", "r1", "c1"));
        msgs2.push(tool_msg("wrong", "files"));
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![b"files"], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 3),
            other => panic!("{other:?}"),
        }
    }

    /// 中途改 `max_tokens` 时按规则 5 失败，即使消息历史仍然衔接。
    #[test]
    fn change_max_tokens_midway_is_rule_5() {
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(assistant_msg("", "r1", "c1"));
        msgs2.push(tool_msg("c1", "files"));
        let mut req2 = base_req(msgs2);
        req2["max_tokens"] = json!(100);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![b"files"], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 5),
            other => panic!("{other:?}"),
        }
    }

    /// 请求带 `tools` 时，assistant 缺 `reasoning_content` 键按规则 1 失败。
    ///
    /// **边界**：空串合法；缺键不合法。
    #[test]
    fn missing_reasoning_content_key_is_rule_1() {
        let req1 = base_req(system_user());
        let resp1 = resp_with_tools("c1", "", "r1");
        let mut msgs2 = system_user();
        msgs2.push(json!({
            "role":"assistant",
            "content":"",
            "tool_calls":[{
                "id":"c1","type":"function",
                "function":{"name":"list_files","arguments":"{}"}
            }]
        }));
        msgs2.push(tool_msg("c1", "files"));
        let req2 = base_req(msgs2);
        let resp2 = resp_stop("done", "r2");
        match verify(&[
            call(1, req1, resp1, vec![b"files"], "tool_calls"),
            call(2, req2, resp2, vec![], "stop"),
        ]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 1),
            other => panic!("{other:?}"),
        }
    }

    /// 事件 `finish_reason` 与响应不一致时按规则 7 失败。
    #[test]
    fn finish_reason_mismatch_is_rule_7() {
        let req1 = base_req(system_user());
        let resp1 = resp_stop("done", "r");
        let mut call1 = call(1, req1, resp1, vec![], "stop");
        call1.event_finish_reason = "length".to_owned();
        match verify(&[call1]) {
            RebuildResult::Inconsistent { rule, .. } => assert_eq!(rule, 7),
            other => panic!("{other:?}"),
        }
    }

    /// 请求 blob 缺失是 `unverifiable`，不得当成 `consistent`。
    ///
    /// **边界**：`missing` 带轮次序号 `request_ref#4`。
    #[test]
    fn missing_request_blob_is_unverifiable() {
        let result = verify(&[RebuildCall {
            index: 4,
            request: None,
            response: Some(resp_stop("x", "")),
            tool_results: vec![],
            event_finish_reason: "stop".to_owned(),
            event_usage: usage_known(),
        }]);
        assert_eq!(
            result,
            RebuildResult::Unverifiable {
                missing: "request_ref#4".into()
            }
        );
    }

    /// 尾轮 `finish_reason=length` 且没有工具事件时为 `consistent`。
    #[test]
    fn last_call_length_with_no_tools_is_consistent() {
        let req = base_req(system_user());
        let resp = json!({
            "content": "partial",
            "reasoning_content": "",
            "tool_calls": [],
            "finish_reason": "length",
            "usage": usage_json(),
        });
        let result = verify(&[call(1, req, resp, vec![], "length")]);
        assert_eq!(result, RebuildResult::Consistent);
    }

    /// 尾轮 `tool_calls` 仅发生前若干次工具调用时，身份和顺序正确即 `consistent`。
    ///
    /// **边界**：保留的工具事件 ID 和工具名必须对应响应的首条调用。
    #[test]
    fn last_call_tool_calls_prefix_is_consistent() {
        let req = base_req(system_user());
        let resp = json!({
            "content": "",
            "reasoning_content": "r",
            "tool_calls": [
                {"id":"a","name":"list_files","arguments":"{}"},
                {"id":"b","name":"list_files","arguments":"{}"}
            ],
            "finish_reason": "tool_calls",
            "usage": usage_json(),
        });
        // 取消在工具批中途：只有第一条工具事件
        let result = verify(&[call(1, req, resp, vec![b"A"], "tool_calls")]);
        assert_eq!(result, RebuildResult::Consistent);
    }

    /// 非对象请求、缺失必需字段与非法消息类型均不可验证，不能靠 None == None 通过。
    #[test]
    fn malformed_request_shapes_are_unverifiable() {
        let valid = base_req(system_user());
        let mut bad_requests = vec![json!(42), Value::Null, json!([]), json!({})];
        for key in ["model", "reasoning_effort", "messages"] {
            let mut req = valid.clone();
            req.as_object_mut().unwrap().remove(key);
            bad_requests.push(req);
        }
        for (key, value) in [
            ("messages", json!([])),
            ("messages", json!([{"role":"system","content":42}])),
            ("tools", json!({})),
            (
                "tools",
                json!([{"type":"function","function":{"name":"x"}}]),
            ),
            ("max_tokens", json!(0)),
            ("max_tokens", Value::Null),
        ] {
            let mut req = valid.clone();
            req[key] = value;
            bad_requests.push(req);
        }
        for request in bad_requests {
            assert_eq!(
                verify(&[call(1, request, resp_stop("done", ""), vec![], "stop")]),
                RebuildResult::Unverifiable {
                    missing: "request_ref#1".into()
                }
            );
        }
    }

    /// 响应工具数组中的任一坏条目不能被过滤；错误须标出 response_ref。
    #[test]
    fn malformed_response_tool_entries_are_unverifiable() {
        for invalid in [
            json!({"id":"extra"}),
            json!(42),
            json!({"id":"","name":"list_files","arguments":"{}"}),
            json!({"id":"x","name":"list_files","arguments":"{"}),
        ] {
            let mut entry = call(
                1,
                base_req(system_user()),
                resp_with_tools("c1", "", "r"),
                vec![],
                "tool_calls",
            );
            entry.response.as_mut().unwrap()["tool_calls"]
                .as_array_mut()
                .unwrap()
                .push(invalid);
            assert_eq!(
                verify(&[entry]),
                RebuildResult::Unverifiable {
                    missing: "response_ref#1".into()
                }
            );
        }
    }

    /// 请求中的嵌套工具列表同样不能过滤坏条目或退回平铺形状。
    #[test]
    fn malformed_assistant_tool_entries_are_unverifiable() {
        let mut messages = system_user();
        let mut assistant = assistant_msg("", "r", "c1");
        assistant["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"extra"}));
        messages.push(assistant);
        messages.push(tool_msg("c1", "files"));
        assert_eq!(
            verify(&[
                call(
                    1,
                    base_req(system_user()),
                    resp_with_tools("c1", "", "r"),
                    vec![b"files"],
                    "tool_calls"
                ),
                call(2, base_req(messages), resp_stop("done", ""), vec![], "stop"),
            ]),
            RebuildResult::Unverifiable {
                missing: "request_ref#2".into()
            }
        );
    }

    /// 相同工具名也必须按调用 ID 排序，尾轮不能跳过第一项只记录第二项。
    #[test]
    fn last_tool_events_must_match_ids_and_names() {
        let response = json!({"content":"", "reasoning_content":"r", "finish_reason":"tool_calls",
            "usage":usage_json(), "tool_calls":[
                {"id":"a","name":"list_files","arguments":"{}"},
                {"id":"b","name":"list_files","arguments":"{}"}]});
        let baseline = call(
            1,
            base_req(system_user()),
            response,
            vec![b"A"],
            "tool_calls",
        );
        for (id, name) in [("b", "list_files"), ("a", "search_code")] {
            let mut entry = baseline.clone();
            entry.tool_results[0].tool_call_id = Some(id.into());
            entry.tool_results[0].tool_name = name.into();
            assert!(matches!(
                verify(&[entry]),
                RebuildResult::Inconsistent { rule: 6, .. }
            ));
        }
        let mut legacy = baseline;
        legacy.tool_results[0].tool_call_id = None;
        assert_eq!(
            verify(&[legacy]),
            RebuildResult::Unverifiable {
                missing: "tool_call_id#1:1".into()
            }
        );
    }

    /// 非尾轮同样核对事件身份，而不仅依赖结果正文；相同正文也不能掩盖错误 ID。
    #[test]
    fn nonlast_tool_identity_mismatch_is_rule_4() {
        let mut messages = system_user();
        messages.push(assistant_msg("", "r", "c1"));
        messages.push(tool_msg("c1", "files"));
        let mut first = call(
            1,
            base_req(system_user()),
            resp_with_tools("c1", "", "r"),
            vec![b"files"],
            "tool_calls",
        );
        first.tool_results[0].tool_call_id = Some("wrong".into());
        assert!(matches!(
            verify(&[
                first,
                call(2, base_req(messages), resp_stop("done", ""), vec![], "stop")
            ]),
            RebuildResult::Inconsistent { rule: 4, .. }
        ));
    }

    /// 传输层记下的失败响应只有 finish_reason 与 usage，仍可以核对已提交事实。
    #[test]
    fn usage_bearing_failure_response_remains_verifiable() {
        let response = json!({"finish_reason":"invalid_tool_arguments", "usage":usage_json()});
        assert_eq!(
            verify(&[call(
                1,
                base_req(system_user()),
                response,
                vec![],
                "invalid_tool_arguments"
            )]),
            RebuildResult::Consistent
        );
        let response = json!({"finish_reason":"content_filter", "usage":usage_json()});
        assert_eq!(
            verify(&[call(
                1,
                base_req(system_user()),
                response,
                vec![],
                "content_filter"
            )]),
            RebuildResult::Unverifiable {
                missing: "response_ref#1".into()
            }
        );
    }
}
