//! Fake Provider 的验收。
//!
//! **作用**：证明缺 reasoning 的 400、空 reasoning、429/5xx 退避、流中断和非法工具参数彼此分开。
//! **边界**：不连接 `api.deepseek.com`，不等待真实时间，也不读取 `DEEPSEEK_API_KEY`。

use std::time::Duration;

use fluxaxiom_contract::{ModelUsage, TokenCount};
use fluxaxiom_engine_custom::{
    execute, observe_reasoning, replay_reasoning, usage_from_json, Backoff, CancellationToken,
    ChatError, ChatRequest, ChatTransport, FakeChat, FakeStep, FinishReason, Message, ObservedText,
    RecordingBackoff, Round, ScriptedTurn, SleepBackoff, ToolCall, ToolSpec, API_KEY_ENV,
    DEFAULT_MODEL, RETRY_DELAYS,
};
use serde_json::json;

/// 未触发的取消令牌。测试默认不取消。
fn open_cancel() -> CancellationToken {
    CancellationToken::new()
}

/// 一份固定用量。缓存命中保持未知，用来确认成功响应不会把未知写成 0。
fn usage() -> ModelUsage {
    ModelUsage {
        prompt_tokens: TokenCount::Known(3),
        completion_tokens: TokenCount::Known(2),
        prompt_cache_hit_tokens: TokenCount::Unknown,
        prompt_cache_miss_tokens: TokenCount::Known(3),
    }
}

/// 构造一步成功响应。
///
/// **作用**：多数测试只需要同一段 delta。
/// **参数**：`finish_reason` 写入这一步的结束原因。`Length` 用来确认截断仍是成功响应。
/// **边界**：不带工具调用。reasoning 由 `"think"` 和 `" more"` 两段拼成。
fn turn(finish_reason: FinishReason) -> FakeStep {
    FakeStep::Turn(ScriptedTurn {
        reasoning_deltas: vec!["think".to_owned(), " more".to_owned()],
        content_deltas: vec!["hello".to_owned()],
        tool_calls: Vec::new(),
        finish_reason,
        usage: usage(),
    })
}

/// 构造一条 assistant 历史消息。
///
/// **作用**：类型只能给出字符串，序列化后键一定在。
/// **参数**：`reasoning` 原样写入 `reasoning_content`。空字符串也保留。
/// **边界**：不带 tool_calls。content 固定为 `"prior"`，测试不靠它判断成败。
fn assistant(reasoning: &str) -> Message {
    Message::Assistant {
        content: "prior".to_owned(),
        reasoning_content: reasoning.to_owned(),
        tool_calls: Vec::new(),
    }
}

/// 用默认模型组装请求。
///
/// **作用**：让测试只改消息，不重复填写模型名和思考强度。
/// **参数**：`messages` 原样成为请求的消息列表。
/// **边界**：不放入密钥，也不写入 `tools` 键。
fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest::new(DEFAULT_MODEL, messages)
}

/// 空的 `reasoning_content` 是合法请求，delta 要拼成完整文本。
///
/// **边界**：不发生重试。响应的 Debug 输出里不出现密钥环境变量名。
#[tokio::test]
async fn empty_reasoning_field_is_accepted_and_deltas_are_joined() {
    let mut fake = FakeChat::script([turn(FinishReason::Stop)]);
    let mut backoff = RecordingBackoff::default();
    let completion = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap();
    assert_eq!(
        completion.reasoning,
        ObservedText::Text("think more".to_owned())
    );
    assert_eq!(completion.content, ObservedText::Text("hello".to_owned()));
    assert!(backoff.waits.is_empty());
    assert!(!format!("{completion:?}").contains(API_KEY_ENV));
}

/// 空字符串必须出现在序列化后的 JSON 里。
///
/// **边界**：只检查 `serde_json::to_string` 的字节。键存在且值为 `""`，不能被省略。
#[test]
fn empty_reasoning_is_serialized_as_an_empty_string() {
    let message = assistant("");
    let text = serde_json::to_string(&message).unwrap();
    assert!(text.contains("\"reasoning_content\":\"\""));
}

/// 带 `tools` 的原始 JSON 若省略 `reasoning_content`，Fake 按假设返回 400。
///
/// **边界**：类型化请求写不出这种报文，所以直接把 JSON 交给 `round`。不消耗后续脚本。
/// 这只验证 Fake 的假设，不证明真实 DeepSeek。依据写在 ADR-020。
#[tokio::test]
async fn raw_json_without_reasoning_and_with_tools_is_400() {
    let mut fake = FakeChat::script([turn(FinishReason::Stop)]);
    let raw = json!({
        "model": DEFAULT_MODEL,
        "messages": [{ "role": "assistant", "content": "prior" }],
        "tools": [{ "name": "read_file", "parameters": {} }]
    });
    let cancel = open_cancel();
    assert_eq!(
        fake.round(&raw, &cancel).await.unwrap(),
        Round::Status {
            status: 400,
            summary: String::new(),
        }
    );
    let mut backoff = RecordingBackoff::default();
    let completion = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Stop);
}

/// 不带 `tools` 时，省略思维链不拒绝。
///
/// **边界**：官方文档说此时即使传入也会被忽略。Fake 消耗脚本并返回成功。
#[tokio::test]
async fn raw_json_without_tools_may_omit_reasoning() {
    let mut fake = FakeChat::script([turn(FinishReason::Stop)]);
    let raw = json!({
        "model": DEFAULT_MODEL,
        "messages": [{ "role": "assistant", "content": "prior" }]
    });
    assert!(matches!(
        fake.round(&raw, &open_cancel()).await.unwrap(),
        Round::Completed(_)
    ));
}

/// 不带工具的请求不写出 `tools` 键。空列表也不写成 `[]`。
///
/// **边界**：默认思考强度是 `high`。这条测试不调用传输层。
#[test]
fn tools_key_is_omitted_unless_a_tool_is_declared() {
    let bare = request(vec![Message::User {
        content: "analyze".to_owned(),
    }]);
    let bare_text = serde_json::to_string(&bare).unwrap();
    assert!(!bare_text.contains("\"tools\""));
    assert!(bare_text.contains("\"reasoning_effort\":\"high\""));
    let still_bare = bare.with_tools(Vec::new());
    assert!(!serde_json::to_string(&still_bare)
        .unwrap()
        .contains("\"tools\""));
    let with_tool = request(Vec::new()).with_tools(vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        parameters: json!({"type": "object"}),
    }]);
    assert!(serde_json::to_string(&with_tool)
        .unwrap()
        .contains("\"tools\""));
}

/// 一次 429 之后重试，下一次成功。
///
/// **边界**：只等待第一档 200ms。`finish_reason = length` 仍是成功响应。
#[tokio::test]
async fn status_429_retries_then_succeeds() {
    let mut fake = FakeChat::script([FakeStep::Status(429), turn(FinishReason::Length)]);
    let mut backoff = RecordingBackoff::default();
    let completion = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Length);
    assert_eq!(backoff.waits, vec![RETRY_DELAYS[0]]);
}

/// 5xx 连续失败时最多重试 3 次。
///
/// **边界**：第四次仍失败后返回 `Unavailable`。间隔是 200ms、400ms、800ms，测试不真正睡眠。
#[tokio::test]
async fn status_503_stops_after_three_retries() {
    let mut fake = FakeChat::script([
        FakeStep::Status(503),
        FakeStep::Status(503),
        FakeStep::Status(503),
        FakeStep::Status(503),
    ]);
    let mut backoff = RecordingBackoff::default();
    let error = execute(
        &mut fake,
        &request(vec![assistant("kept")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        ChatError::Unavailable {
            status: 503,
            summary: String::new(),
        }
    );
    assert_eq!(
        backoff.waits,
        vec![
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
        ]
    );
}

/// Provider 返回的 400 不进入退避。
///
/// **边界**：脚本里后面的成功步不会被取走。
#[tokio::test]
async fn status_400_from_provider_does_not_retry() {
    let mut fake = FakeChat::script([FakeStep::Status(400), turn(FinishReason::Stop)]);
    let mut backoff = RecordingBackoff::default();
    let error = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        ChatError::Rejected {
            status: 400,
            summary: String::new(),
        }
    );
    assert!(backoff.waits.is_empty());
}

/// 流中断视为这一次调用失败。
///
/// **边界**：不重试，也不把中断当成 5xx。
#[tokio::test]
async fn stream_interrupt_does_not_retry() {
    let mut fake = FakeChat::script([FakeStep::Interrupt, turn(FinishReason::Stop)]);
    let mut backoff = RecordingBackoff::default();
    let error = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, ChatError::StreamInterrupted);
    assert!(backoff.waits.is_empty());
}

/// 退避期间令牌被触发后，`execute` 不再进入下一次 `round`。
///
/// **边界**：429 之后 `wait` 先记下间隔再触发令牌并返回 `Ok`，所以停下的是 `execute` 自己的下一次 `round`，不是 `wait` 的错误传播。
/// 返回 `Cancelled`，不是 `StreamInterrupted`。脚本里的成功步没被取走。
#[tokio::test]
async fn cancel_during_backoff_stops_before_the_next_round() {
    let mut fake = FakeChat::script([FakeStep::Status(429), turn(FinishReason::Stop)]);
    let cancel = open_cancel();
    let mut backoff = CancelAfterWait { waits: Vec::new() };
    let error = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(error, ChatError::Cancelled);
    assert_eq!(backoff.waits, vec![RETRY_DELAYS[0]]);
    let mut leftover = RecordingBackoff::default();
    let completion = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut leftover,
        &open_cancel(),
    )
    .await
    .unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Stop);
}

/// 记下间隔、触发令牌，然后照常返回 `Ok`，模拟取消恰好发生在等待结束的瞬间。
struct CancelAfterWait {
    waits: Vec<Duration>,
}

#[async_trait::async_trait]
impl fluxaxiom_engine_custom::Backoff for CancelAfterWait {
    /// **作用**：把间隔记入 `waits`，再取消传入的令牌，让 `execute` 靠下一次 `round` 发现取消。
    /// **参数**：`delay` 记入 `waits`。`cancel` 是 `execute` 手里的同一个令牌。
    /// **边界**：不睡眠，永远返回 `Ok`。
    async fn wait(&mut self, delay: Duration, cancel: &CancellationToken) -> Result<(), ChatError> {
        self.waits.push(delay);
        cancel.cancel();
        Ok(())
    }
}

/// 令牌在 `round` 之前触发时，立刻返回 `Cancelled` 且不消耗脚本。
///
/// **边界**：不访问网络。后面的成功步仍在脚本里。
#[tokio::test]
async fn cancelled_round_does_not_consume_the_script() {
    let mut fake = FakeChat::script([turn(FinishReason::Stop)]);
    let cancel = open_cancel();
    cancel.cancel();
    let mut backoff = RecordingBackoff::default();
    let error = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(error, ChatError::Cancelled);
    assert!(backoff.waits.is_empty());
    let open = open_cancel();
    let completion = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open,
    )
    .await
    .unwrap();
    assert_eq!(completion.finish_reason, FinishReason::Stop);
}

/// `RecordingBackoff` 在令牌已触发时不记录间隔。
///
/// **边界**：返回 `Cancelled`，`waits` 保持为空。
#[tokio::test]
async fn recording_backoff_refuses_a_cancelled_token() {
    let cancel = open_cancel();
    cancel.cancel();
    let mut backoff = RecordingBackoff::default();
    let result = backoff.wait(RETRY_DELAYS[0], &cancel).await;
    assert_eq!(result, Err(ChatError::Cancelled));
    assert!(backoff.waits.is_empty());
}

/// `SleepBackoff` 睡满后返回 `Ok`，令牌触发时提前返回 `Cancelled`。
///
/// **边界**：睡眠用 1ms 的真实计时器。取消分支用 1 小时的间隔，如果没有提前返回，测试会超时。
#[tokio::test]
async fn sleep_backoff_waits_and_is_interruptible() {
    let mut backoff = SleepBackoff;
    assert_eq!(
        backoff.wait(Duration::from_millis(1), &open_cancel()).await,
        Ok(())
    );
    let cancel = open_cancel();
    let trigger = cancel.clone();
    tokio::spawn(async move { trigger.cancel() });
    assert_eq!(
        backoff.wait(Duration::from_secs(3600), &cancel).await,
        Err(ChatError::Cancelled)
    );
}

/// `execute` 的 future 必须是 `Send`，否则放不进 `EngineAdapter::run`。
///
/// **边界**：只在编译期检查，不运行。`ChatTransport` 与 `Backoff` 少了 `Send` 约束时这里编译失败。
#[test]
fn execute_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let mut fake = FakeChat::script([]);
    let mut backoff = RecordingBackoff::default();
    let request = request(vec![assistant("")]);
    let cancel = open_cancel();
    let future = execute(&mut fake, &request, &mut backoff, &cancel);
    assert_send(&future);
}

/// 工具参数不是 JSON 时立即失败。
///
/// **边界**：不重试。`arguments` 使用 `"{"`，它不是完整 JSON。
#[tokio::test]
async fn invalid_tool_arguments_do_not_retry() {
    let mut fake = FakeChat::script([FakeStep::Turn(ScriptedTurn {
        reasoning_deltas: vec![String::new()],
        content_deltas: vec![String::new()],
        tool_calls: vec![ToolCall {
            id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{".to_owned(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(),
    })]);
    let mut backoff = RecordingBackoff::default();
    let error = execute(
        &mut fake,
        &request(vec![assistant("")]),
        &mut backoff,
        &open_cancel(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, ChatError::InvalidToolArguments);
    assert!(backoff.waits.is_empty());
}

/// 用量缺字段是未知，数字 0 仍是已知的 0。
///
/// **边界**：不把缺失的缓存字段写成 0。这条测试只解析 JSON，不调用 Fake。
#[test]
fn missing_cache_tokens_are_unknown_and_zero_is_known() {
    let usage = usage_from_json(&json!({
        "prompt_tokens": 0,
        "completion_tokens": "unknown"
    }));
    assert_eq!(usage.prompt_tokens, TokenCount::Known(0));
    assert_eq!(usage.completion_tokens, TokenCount::Unknown);
    assert_eq!(usage.prompt_cache_hit_tokens, TokenCount::Unknown);
    assert_eq!(usage.prompt_cache_miss_tokens, TokenCount::Unknown);
}

/// 缺键和 `null` 不编成空串。带工具时回传才补 `""`，并标明是 Host 补的。
///
/// **边界**：不带工具时不回传思维链。原文空串不标记为补写。
#[test]
fn replay_does_not_invent_an_empty_string_without_recording_it() {
    assert_eq!(
        observe_reasoning(&json!({"role": "assistant", "content": ""})),
        ObservedText::Absent
    );
    assert_eq!(
        observe_reasoning(&json!({"reasoning_content": null})),
        ObservedText::Absent
    );
    let filled = replay_reasoning(ObservedText::Absent, true);
    assert_eq!(filled.reasoning_content.as_deref(), Some(""));
    assert!(filled.filled_by_host);
    let kept = replay_reasoning(ObservedText::Empty, true);
    assert!(!kept.filled_by_host);
    let omitted = replay_reasoning(ObservedText::Text("think".to_owned()), false);
    assert_eq!(omitted.reasoning_content, None);
    assert!(!format!("{omitted:?}").contains("think"));
    assert!(!format!("{:?}", ObservedText::Text("think".to_owned())).contains("think"));
}

/// 工具声明、工具调用和 tool 消息必须是思考模式样例里的嵌套 JSON。
///
/// **边界**：断言的是 `serde_json::to_value` 的结果，不是结构体字段是否存在。
#[test]
fn tool_wire_format_is_nested_function_objects() {
    let request = request(vec![
        Message::Assistant {
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{}".to_owned(),
            }],
        },
        Message::Tool {
            content: "file body".to_owned(),
            tool_call_id: "call_1".to_owned(),
        },
    ])
    .with_tools(vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        parameters: json!({"type": "object", "properties": {}}),
    }]);
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        json["tools"][0],
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object", "properties": {}}
            }
        })
    );
    assert_eq!(
        json["messages"][0]["tool_calls"][0],
        json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        })
    );
    assert_eq!(
        json["messages"][1],
        json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "file body"
        })
    );
}

/// 负数和小数不能变成已知的 0。
///
/// **边界**：无法放进 `u64` 的数字记为 `Unknown`。
#[test]
fn non_integer_token_counts_stay_unknown() {
    let usage = usage_from_json(&json!({
        "prompt_tokens": -1,
        "completion_tokens": 1.5
    }));
    assert_eq!(usage.prompt_tokens, TokenCount::Unknown);
    assert_eq!(usage.completion_tokens, TokenCount::Unknown);
}

/// Debug 不输出思维链和正文。
///
/// **边界**：检查的是格式化后的文本里没有原文，而不是没有密钥变量名。
#[test]
fn debug_redacts_reasoning_and_content() {
    let message = assistant("secret-thought");
    let completion = fluxaxiom_engine_custom::ChatCompletion {
        reasoning: ObservedText::Text("secret-thought".to_owned()),
        content: ObservedText::Text("secret-answer".to_owned()),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: usage(),
    };
    let rendered = format!("{message:?} {completion:?}");
    assert!(!rendered.contains("secret-thought"));
    assert!(!rendered.contains("secret-answer"));
    assert!(!rendered.contains("prior"));
}
