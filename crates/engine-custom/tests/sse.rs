//! SSE 解析与客户端构造。
//!
//! **作用**：用手写的 SSE 文本证明思维链和正文会分开拼接，缺密钥不会发出请求。
//! **边界**：这些样本是我们的假设，不是从 DeepSeek 录下来的响应。真实录到一份后再换掉。默认测试不访问 `api.deepseek.com`。`data:` 分帧没有出现在思考模式页面的原始样例里，字段名来自该页的 Python 循环。

use fluxaxiom_contract::TokenCount;
use fluxaxiom_engine_custom::{
    parse_sse, stream_request_body, CancellationToken, ChatError, DeepSeekClient, FinishReason,
    ObservedText, API_KEY_ENV,
};

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

/// 非 null 的工具分片这一刀直接拒绝，避免把没解析的调用当成没有工具。
///
/// **边界**：`tool_calls: null` 不是工具分片，正文仍可完成。样本是手写的。
#[test]
fn tool_call_delta_is_unsupported() {
    let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]},\"finish_reason\":null}]}\n\
data: [DONE]\n";
    assert_eq!(parse_sse(body).unwrap_err(), ChatError::UnsupportedDelta);
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
