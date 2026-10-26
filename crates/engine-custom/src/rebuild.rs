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
/// **边界**：任一 `*_json` 为 `None` 表示对应 blob 读不到或不是 JSON；工具结果同理。
#[derive(Debug, Clone)]
pub struct RebuildCall {
    /// 从 1 起的模型轮次序号（与 `flux trace` 一致）。
    pub index: u32,
    /// 请求 blob 解析结果；缺省为 unverifiable。
    pub request: Option<Value>,
    /// 响应 blob 解析结果。
    pub response: Option<Value>,
    /// 该轮 `tool.call_completed` 对应的正文，与事件顺序一致；`None` 表示缺 blob。
    pub tool_results: Vec<Option<Vec<u8>>>,
    /// 事件里的 `finish_reason`。
    pub event_finish_reason: String,
    /// 事件里的 `usage`。
    pub event_usage: ModelUsage,
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
        if call.request.is_none() {
            return RebuildResult::Unverifiable {
                missing: format!("request_ref#{}", call.index),
            };
        }
        if call.response.is_none() {
            return RebuildResult::Unverifiable {
                missing: format!("response_ref#{}", call.index),
            };
        }
        for (j, tool) in call.tool_results.iter().enumerate() {
            if tool.is_none() {
                return RebuildResult::Unverifiable {
                    missing: format!("tool_result#{}:{}", call.index, j + 1),
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
        let resp_tool_calls = resp_tool_calls(resp);
        let tool_event_count = call.tool_results.len();

        if is_last {
            // 规则 6：尾轮。
            if call.event_finish_reason == "tool_calls" {
                if tool_event_count > resp_tool_calls.len() {
                    return inconsistent(k, 6, "last call tool events exceed response tool_calls");
                }
                if let Err(detail) =
                    tools_are_prefix(&resp_tool_calls, &call.tool_results, req_has_tools, resp)
                {
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
/// **作用**：`flux trace --verify` 追加行；`--json` 不用本函数。
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

fn inconsistent(call: u32, rule: u8, detail: impl Into<String>) -> RebuildResult {
    RebuildResult::Inconsistent {
        call,
        rule,
        detail: detail.into(),
    }
}

fn first_system_message(req: &Value) -> Option<&Value> {
    req.get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.first())
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
}

/// 检查 `next.messages == prev.messages ++ [assistant] ++ tools`（规则 1–3）。
fn check_history_extension(
    prev: &Value,
    next: &Value,
    resp: &Value,
    tool_results: &[Option<Vec<u8>>],
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
    let resp_calls = resp_tool_calls(resp);
    for (j, tool_msg) in suffix[1..].iter().enumerate() {
        check_tool_message(tool_msg, &resp_calls, j, tool_results[j].as_ref().unwrap())?;
    }
    Ok(())
}

/// 规则 1–2：assistant 与响应字段对齐。
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
    let assistant_calls = assistant_tool_calls(assistant);
    let resp_calls = resp_tool_calls(resp);
    if assistant_calls != resp_calls {
        return Err((2, "assistant.tool_calls != response.tool_calls".into()));
    }
    Ok(())
}

/// 规则 3：tool 消息的 id 与正文。
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

/// 尾轮：工具事件是响应 tool_calls 的前缀（规则 6）。
fn tools_are_prefix(
    resp_calls: &[FlatToolCall],
    tool_results: &[Option<Vec<u8>>],
    _req_has_tools: bool,
    _resp: &Value,
) -> Result<(), String> {
    if tool_results.len() > resp_calls.len() {
        return Err("tool events longer than tool_calls".into());
    }
    Ok(())
}

fn messages_array(req: &Value) -> Option<&[Value]> {
    req.get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
}

fn text_field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn resp_tool_calls(resp: &Value) -> Vec<FlatToolCall> {
    resp.get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    Some(FlatToolCall {
                        id: item.get("id")?.as_str()?.to_owned(),
                        name: item.get("name")?.as_str()?.to_owned(),
                        arguments: item.get("arguments")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn assistant_tool_calls(assistant: &Value) -> Vec<FlatToolCall> {
    assistant
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    // 请求里是嵌套 OpenAI 形状。
                    if let (Some(id), Some(name), Some(arguments)) = (
                        item.get("id").and_then(Value::as_str),
                        item.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str),
                        item.get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str),
                    ) {
                        return Some(FlatToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments: arguments.to_owned(),
                        });
                    }
                    // 也接受响应那种平铺形状，便于手写反例。
                    Some(FlatToolCall {
                        id: item.get("id")?.as_str()?.to_owned(),
                        name: item.get("name")?.as_str()?.to_owned(),
                        arguments: item.get("arguments")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

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

    fn usage_known() -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(10),
            completion_tokens: TokenCount::Known(5),
            prompt_cache_hit_tokens: TokenCount::Known(0),
            prompt_cache_miss_tokens: TokenCount::Known(10),
        }
    }

    fn usage_json() -> Value {
        json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 10,
        })
    }

    fn base_req(messages: Vec<Value>) -> Value {
        json!({
            "model": "deepseek-flash",
            "reasoning_effort": "high",
            "max_tokens": 32768,
            "tools": [{"type":"function","function":{"name":"list_files","description":"","parameters":{}}}],
            "messages": messages,
        })
    }

    fn system_user() -> Vec<Value> {
        vec![
            json!({"role":"system","content":"sys"}),
            json!({"role":"user","content":"task"}),
        ]
    }

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

    fn tool_msg(id: &str, body: &str) -> Value {
        json!({"role":"tool","tool_call_id": id, "content": body})
    }

    fn call(
        index: u32,
        request: Value,
        response: Value,
        tools: Vec<&[u8]>,
        finish: &str,
    ) -> RebuildCall {
        RebuildCall {
            index,
            request: Some(request),
            response: Some(response),
            tool_results: tools.into_iter().map(|b| Some(b.to_vec())).collect(),
            event_finish_reason: finish.to_owned(),
            event_usage: usage_known(),
        }
    }

    /// 正向：两轮工具 + stop。
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
}
