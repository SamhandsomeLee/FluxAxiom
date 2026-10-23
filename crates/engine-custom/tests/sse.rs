//! SSE 解析与客户端构造。
//!
//! **作用**：用手写的 SSE 文本证明思维链和正文会分开拼接，缺密钥不会发出请求。
//! **边界**：这些样本是我们的假设，不是从 DeepSeek 录下来的响应。真实录到一份后再换掉。默认测试不访问 `api.deepseek.com`。`data:` 分帧没有出现在思考模式页面的原始样例里，字段名来自该页的 Python 循环。

use fluxaxiom_contract::TokenCount;
use fluxaxiom_engine_custom::{
    parse_sse, stream_request_body, CancellationToken, ChatError, DeepSeekClient, FinishReason,
    ObservedText, ToolCall, API_KEY_ENV,
};
use serde_json::json;

const COMPLETE_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" more\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n\
data: [DONE]\n";

/// 文档样例里的两种 delta 要拼进不同字段。
///
/// **边界**：usage 里没出现的缓存字段保持未知，不写成 0。
#[test]
fn handwritten_stream_joins_reasoning_and_content() {
    let completion = parse_sse(COMPLETE_STREAM).unwrap();
    assert_eq!(
        completion.reasoning,
        ObservedText::Text("think more".to_owned())
    );
    assert_eq!(completion.content, ObservedText::Text("hello".to_owned()));
    assert_eq!(completion.finish_reason, FinishReason::Stop);
    assert_eq!(completion.usage.prompt_tokens, TokenCount::Known(1));
    assert_eq!(
        completion.usage.prompt_cache_hit_tokens,
        TokenCount::Unknown
    );
}

/// 没有结束标记时不返回已拼的片段。
///
/// **边界**：错误值里没有 `partial` 文本。
#[test]
fn stream_without_done_is_interrupted() {
    let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"partial\"}}]}\n";
    let error = parse_sse(body).unwrap_err();
    assert_eq!(error, ChatError::StreamInterrupted);
    assert!(!error.to_string().contains("partial"));
}

/// 中间块的 `usage: null` 不能把末块的计数冲掉。
///
/// **边界**：`include_usage: true` 时，除最后一块外 usage 都是 null。只有对象才记下来。
#[test]
fn null_usage_chunks_do_not_replace_the_final_object() {
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}],\"usage\":null}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\
data: [DONE]\n";
    let completion = parse_sse(body).unwrap();
    assert_eq!(completion.usage.prompt_tokens, TokenCount::Known(4));
    assert_eq!(completion.usage.completion_tokens, TokenCount::Known(1));
}

/// `finish_reason = length` 仍是一次完成的响应。
#[test]
fn length_finish_is_a_completed_stream() {
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"cut\"},\"finish_reason\":\"length\"}]}\n\
data: [DONE]\n";
    let completion = parse_sse(body).unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Length);
    assert_eq!(completion.reasoning, ObservedText::Absent);
}

/// 把若干 chunk 对象拼成 SSE 正文，末尾带 `[DONE]`。
///
/// **参数**：`chunks` 按到达顺序排列，每个对象成为一行 `data:`。
/// **边界**：用 `serde_json` 序列化，避免手写转义。`with_done` 为假时不写 `[DONE]`，用来模拟中途断开。
fn sse_body(chunks: &[serde_json::Value], with_done: bool) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push('\n');
    }
    if with_done {
        body.push_str("data: [DONE]\n");
    }
    body
}

/// 输出触顶时工具参数被截断，仍是 `Length` 的完成结果。
///
/// **边界**：不是 `InvalidToolArguments`。`tool_calls` 为空，不把截断的调用交给调用方；已到达的 `content` 和用量保留，Loop 才能凭 `Length` 决定终态。样本是手写的。
#[test]
fn length_with_truncated_tool_arguments_is_a_length_completion() {
    let body = sse_body(
        &[
            json!({"choices":[{"delta":{"content":"partial","tool_calls":[
                {"index":0,"id":"call_1","type":"function",
                 "function":{"name":"read_file","arguments":"{\"path\":\"a"}}
            ]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}],
                   "usage":{"prompt_tokens":1,"completion_tokens":32768}}),
        ],
        true,
    );
    let completion = parse_sse(&body).unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Length);
    assert!(completion.tool_calls.is_empty());
    assert_eq!(completion.content, ObservedText::Text("partial".to_owned()));
    assert_eq!(completion.usage.completion_tokens, TokenCount::Known(32768));
}

/// 输出触顶时，缺 id、type 不对这类分片问题同样不报错。
///
/// **边界**：触顶可能发生在 `id` 到达之前，所以这不是服务端违约。`tool_calls` 为空。同样的分片在 `finish_reason = tool_calls` 时仍是 `MalformedToolCall`，见 `malformed_tool_calls_are_not_stream_interruptions`。
#[test]
fn length_ignores_malformed_tool_fragments() {
    let fragments = [
        json!({"index":0,"function":{"name":"read_file","arguments":"{}"}}),
        json!({"index":0,"id":"call_1","type":"custom","function":{"name":"read_file"}}),
        json!({"id":"call_1","function":{"name":"read_file"}}),
    ];
    for fragment in fragments {
        let body = sse_body(
            &[
                json!({"choices":[{"delta":{"tool_calls":[fragment]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            ],
            true,
        );
        let completion = parse_sse(&body).unwrap();
        assert_eq!(completion.finish_reason, FinishReason::Length);
        assert!(completion.tool_calls.is_empty());
    }
}

/// 分片错误要等读完整个流才报告，而且流中断优先。
///
/// **边界**：同一份畸形分片，没有 `[DONE]` 时是 `StreamInterrupted`，不是 `MalformedToolCall`；有 `[DONE]` 且结束原因是 `tool_calls` 时才是后者。
#[test]
fn tool_fragment_errors_wait_for_the_end_of_the_stream() {
    let chunks = [
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"read_file","arguments":"{}"}}
        ]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ];
    assert_eq!(
        parse_sse(&sse_body(&chunks, false)).unwrap_err(),
        ChatError::StreamInterrupted
    );
    assert_eq!(
        parse_sse(&sse_body(&chunks, true)).unwrap_err(),
        ChatError::MalformedToolCall
    );
}

/// 流式工具分片按 index 拼接成完整调用。
///
/// **边界**：手写样本，编码 create-chat-completion chunk schema 的假设，不是从 DeepSeek 录下来的。首块带 id/name，后续只带 arguments 片段。
#[test]
fn handwritten_stream_joins_tool_call_fragments() {
    let body = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
        "\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]},"finish_reason":null}]}"#,
        "\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"list_files","arguments":"{}"}}]},"finish_reason":null}]}"#,
        "\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#,
        "\n",
        "data: [DONE]\n",
    );
    let completion = parse_sse(body).unwrap();
    assert_eq!(completion.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        completion.tool_calls,
        vec![
            ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a.rs\"}".to_owned(),
            },
            ToolCall {
                id: "call_2".to_owned(),
                name: "list_files".to_owned(),
                arguments: "{}".to_owned(),
            },
        ]
    );
}

/// `tool_calls: null` 不把普通文本块判成工具分片。
///
/// **边界**：手写样本。没有证据表明 DeepSeek 会这样发，兼容实现可能会。
#[test]
fn null_tool_calls_do_not_reject_the_stream() {
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":null},\"finish_reason\":\"stop\"}]}\n\
data: [DONE]\n";
    let completion = parse_sse(body).unwrap();
    assert_eq!(completion.content, ObservedText::Text("hi".to_owned()));
    assert!(completion.tool_calls.is_empty());
}

/// 缺 index、缺 id、空 name、同 index 冲突、同响应 id 重复都是畸形，不是流中断。
///
/// **边界**：错误值里没有半截 arguments。样本是手写的。
#[test]
fn malformed_tool_calls_are_not_stream_interruptions() {
    let missing_index = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(missing_index).unwrap_err(),
        ChatError::MalformedToolCall
    );

    let missing_id = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(missing_id).unwrap_err(),
        ChatError::MalformedToolCall
    );

    let empty_name = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(empty_name).unwrap_err(),
        ChatError::MalformedToolCall
    );

    let conflict = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(conflict).unwrap_err(),
        ChatError::MalformedToolCall
    );

    let duplicate_id = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"call_1\",\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(duplicate_id).unwrap_err(),
        ChatError::MalformedToolCall
    );

    let bad_type = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"custom","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        "\n",
        "data: [DONE]\n",
    );
    assert_eq!(
        parse_sse(bad_type).unwrap_err(),
        ChatError::MalformedToolCall
    );
}

/// 拼完后的 arguments 不是 JSON（含空串）是 `InvalidToolArguments`，先过畸形检查。
///
/// **边界**：id 与 name 齐全。不重试。样本是手写的。
#[test]
fn invalid_tool_arguments_after_join_fail_the_stream() {
    let empty_args = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(empty_args).unwrap_err(),
        ChatError::InvalidToolArguments
    );

    let broken = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    assert_eq!(
        parse_sse(broken).unwrap_err(),
        ChatError::InvalidToolArguments
    );
}

/// 未知结束原因保留原文，但 `Debug` 不打印它。
///
/// **边界**：手写样本。空的 `finish_reason` 不算结束。
#[test]
fn unknown_finish_reason_keeps_the_provider_text() {
    let body = "\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\
data: [DONE]\n";
    let completion = parse_sse(body).unwrap();
    assert_eq!(
        completion.finish_reason,
        FinishReason::Other("content_filter".to_owned())
    );
    assert!(!format!("{completion:?}").contains("content_filter"));
}

/// 空密钥在构造时失败。
///
/// **参数**：传入的密钥只用于证明它不会出现在错误文本和 `Debug` 里。
/// **边界**：不发起 HTTP。
#[test]
fn empty_api_key_does_not_build_a_client() {
    let error = DeepSeekClient::from_key("   ", "https://api.deepseek.com").unwrap_err();
    assert_eq!(error, ChatError::MissingCredential);
    assert!(!error.to_string().contains(API_KEY_ENV));
    let client =
        DeepSeekClient::from_key("super-secret-key\n", "https://api.deepseek.com").unwrap();
    let rendered = format!("{client:?}");
    assert!(!rendered.contains("super-secret-key"));
    assert!(rendered.contains(&format!("api_key_len: {}", "super-secret-key".len())));
}

/// 手动核对真实 API。默认 `cargo test` 不跑。
///
/// **边界**：没有密钥时直接返回，不拨号。有密钥时才会访问 DeepSeek。取消令牌默认不触发，退避用真实睡眠。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_completion_when_key_present() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        return;
    }
    let mut client = DeepSeekClient::from_env().unwrap();
    let request = fluxaxiom_engine_custom::ChatRequest::new(
        fluxaxiom_engine_custom::DEFAULT_MODEL,
        vec![fluxaxiom_engine_custom::Message::User {
            content: "Reply with the single word pong.".to_owned(),
        }],
    );
    let mut backoff = fluxaxiom_engine_custom::SleepBackoff;
    let cancel = CancellationToken::new();
    let completion = fluxaxiom_engine_custom::execute(&mut client, &request, &mut backoff, &cancel)
        .await
        .expect("live call");
    assert!(matches!(completion.content, ObservedText::Text(_)));
    assert!(
        matches!(completion.usage.prompt_tokens, TokenCount::Known(_)),
        "stream usage was not returned"
    );
    assert!(!format!("{completion:?}").contains("pong"));
}

/// 带一个工具的真实流。用来以后把录到的 SSE 换掉手写样本。
///
/// **边界**：没有密钥时直接返回。默认 `cargo test` 不跑。断言至少收到一条工具调用；不把 arguments 正文写进断言消息。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set; records tool_calls stream"]
async fn live_tool_call_stream_when_key_present() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        return;
    }
    let mut client = DeepSeekClient::from_env().unwrap();
    let request = fluxaxiom_engine_custom::ChatRequest::new(
        fluxaxiom_engine_custom::DEFAULT_MODEL,
        vec![fluxaxiom_engine_custom::Message::User {
            content: "Call list_files once with path \".\" and then stop.".to_owned(),
        }],
    )
    .with_tools(vec![fluxaxiom_engine_custom::ToolSpec {
        name: "list_files".to_owned(),
        description: "List files under a relative path.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    }]);
    let mut backoff = fluxaxiom_engine_custom::SleepBackoff;
    let cancel = CancellationToken::new();
    let completion = fluxaxiom_engine_custom::execute(&mut client, &request, &mut backoff, &cancel)
        .await
        .expect("live tool call");
    assert!(
        !completion.tool_calls.is_empty(),
        "expected at least one tool call from the live stream"
    );
    assert_eq!(completion.finish_reason, FinishReason::ToolCalls);
}

/// 真实请求要显式打开 usage，不能只靠末块默认带统计。
///
/// **边界**：不发网络请求。断言的是 `round` 复制出来的请求体。
#[test]
fn stream_request_asks_for_usage() {
    let body = stream_request_body(&serde_json::json!({
        "model": fluxaxiom_engine_custom::DEFAULT_MODEL,
        "messages": [{ "role": "user", "content": "ping" }]
    }))
    .unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}
