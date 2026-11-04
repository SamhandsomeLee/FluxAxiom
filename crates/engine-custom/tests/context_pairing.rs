//! T10 配对探针。
//!
//! **作用**：检查四条请求的 tool 消息形状，并在显式实调时记录接受或拒绝。
//! **边界**：默认测试不拨号，不写 Run、事件或档案。只有 `#[ignore]` 的测试连接 DeepSeek。stdout 不含消息正文、错误摘要或密钥。

use fluxaxiom_engine_custom::{
    execute, CancellationToken, ChatError, ChatRequest, DeepSeekClient, Message, ReasoningEffort,
    SleepBackoff, ToolCall, ToolSpec, API_KEY_ENV, DEFAULT_MODEL,
};
use serde_json::{json, Value};

/// 对照和错配用例的 tool 正文。
const PLAIN_CONTENT: &str = "ok";

/// 成对占位的 tool 正文。只测这一句 L0 存根。
const STUB_CONTENT: &str = "[unchanged: same result as round #1 call 1]";

/// assistant 工具调用使用的 id。
const CALL_ID: &str = "call_1";

/// 故意对不上的 tool 消息 id。
const OTHER_CALL_ID: &str = "call_2";

/// 四条请求共用的 system 正文。
const SYSTEM_CONTENT: &str = "reply in one word";

/// 四条请求共用的 user 正文。
const USER_CONTENT: &str = "prefix";

/// 一次配对探针。
///
/// **作用**：选择四条请求里的 tool 消息。序列化测试和实调共用。
/// **边界**：不表示服务端结果。正文和 id 都是固定短字符串，不会写入 stdout。
#[derive(Clone, Copy)]
enum PairingCase {
    /// id 为 `call_1`，正文是 [`PLAIN_CONTENT`]。
    PairedPlain,
    /// id 为 `call_1`，正文是 [`STUB_CONTENT`]。
    PairedStub,
    /// 不追加 tool 消息。
    MissingTool,
    /// id 为 `call_2`，正文是 [`PLAIN_CONTENT`]。
    MismatchedId,
}

/// 用例在 stdout 里的短名字。
///
/// **作用**：实调输出用它区分四次请求。
/// **参数**：`case` 选择哪一次探针。四个变体都有固定名字。
/// **边界**：不包含正文或 id。
fn case_name(case: PairingCase) -> &'static str {
    match case {
        PairingCase::PairedPlain => "paired_plain",
        PairingCase::PairedStub => "paired_stub",
        PairingCase::MissingTool => "missing_tool",
        PairingCase::MismatchedId => "mismatched_id",
    }
}

/// 这一次要追加的 tool 消息。
///
/// **作用**：让四条请求只在最后一条 tool 消息上不同。
/// **参数**：`case` 选择正文和 id。`MissingTool` 不产生消息。
/// **边界**：返回的 id 只有 `call_1` 或 `call_2`。不读取文件，不改 assistant 上的调用。
fn tool_message_of(case: PairingCase) -> Option<(&'static str, &'static str)> {
    match case {
        PairingCase::PairedPlain => Some((CALL_ID, PLAIN_CONTENT)),
        PairingCase::PairedStub => Some((CALL_ID, STUB_CONTENT)),
        PairingCase::MissingTool => None,
        PairingCase::MismatchedId => Some((OTHER_CALL_ID, PLAIN_CONTENT)),
    }
}

/// 组装一条配对探针请求。
///
/// **作用**：序列化测试和实调都从这里拿请求。
/// **参数**：`case` 决定是否追加 tool 消息，以及它的 id 和正文。
/// **边界**：模型是 `deepseek-flash`。`reasoning_effort` 为 `Low`，`max_tokens` 为 1。assistant 的 `reasoning_content` 是空字符串，键仍在。工具声明只有 `ping`。不拨号。
fn pairing_request(case: PairingCase) -> ChatRequest {
    let mut messages = vec![
        Message::System {
            content: SYSTEM_CONTENT.to_owned(),
        },
        Message::User {
            content: USER_CONTENT.to_owned(),
        },
        Message::Assistant {
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: vec![ToolCall {
                id: CALL_ID.to_owned(),
                name: "ping".to_owned(),
                arguments: "{}".to_owned(),
            }],
        },
    ];
    if let Some((id, content)) = tool_message_of(case) {
        messages.push(Message::Tool {
            content: content.to_owned(),
            tool_call_id: id.to_owned(),
        });
    }
    let mut request = ChatRequest::new(DEFAULT_MODEL, messages)
        .with_tools(vec![ToolSpec {
            name: "ping".to_owned(),
            description: "Reply with ok.".to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
        }])
        .with_max_tokens(1)
        .expect("max_tokens 1 is valid");
    request.reasoning_effort = ReasoningEffort::Low;
    request
}

/// 失败时打印的状态标签。
///
/// **作用**：stdout 只留类别和状态码。
/// **参数**：`error` 是一次 `execute` 失败。`Rejected` 和 `Unavailable` 带上状态码。
/// **边界**：不打印 `summary`、正文或密钥。成功路径不使用这个函数。
fn http_label(error: &ChatError) -> String {
    match error {
        ChatError::Rejected { status, .. } => format!("rejected:{status}"),
        ChatError::Unavailable { status, .. } => format!("unavailable:{status}"),
        ChatError::StreamInterrupted => "stream_interrupted".to_owned(),
        ChatError::Cancelled => "cancelled".to_owned(),
        ChatError::InvalidToolArguments { .. } => "invalid_tool_arguments".to_owned(),
        ChatError::MalformedToolCall { .. } => "malformed_tool_call".to_owned(),
        ChatError::NotEncodable => "not_encodable".to_owned(),
        ChatError::MissingCredential => "missing_credential".to_owned(),
        ChatError::Transport => "transport".to_owned(),
    }
}

/// 发送一条探针并返回状态标签。
///
/// **作用**：实调按顺序调用它。
/// **参数**：`client` 是已带密钥的 DeepSeek 客户端，本次调用会使用它。`case` 选择要发送的请求。
/// **边界**：`Ok` 一律记为 `completed`，不看结束原因。不打印响应正文。400 不在这里重试，429 和 5xx 仍由 `execute` 退避。
async fn pairing_label(client: &mut DeepSeekClient, case: PairingCase) -> String {
    let request = pairing_request(case);
    match execute(
        client,
        &request,
        &mut SleepBackoff,
        &CancellationToken::new(),
    )
    .await
    {
        Ok(_) => "completed".to_owned(),
        Err(error) => http_label(&error),
    }
}

/// 打印一次探针的状态。
///
/// **作用**：实调的 stdout 只留用例名和状态。
/// **参数**：`case` 是这一次探针。`label` 是 [`pairing_label`] 返回的状态，不含摘要。
/// **边界**：占位用例多写字符数。不写存根正文。
fn print_case(case: PairingCase, label: &str) {
    match case {
        PairingCase::PairedStub => {
            println!(
                "case={} http={label} chars={}",
                case_name(case),
                STUB_CONTENT.chars().count()
            );
        }
        _ => println!("case={} http={label}", case_name(case)),
    }
}

/// 四条请求的前三条消息。
///
/// **作用**：序列化测试确认 system、user 和 assistant 不随用例变化。
/// **边界**：assistant 带一条 `ping` 调用，`reasoning_content` 为空字符串。
fn shared_messages() -> Value {
    json!([
        {"role": "system", "content": SYSTEM_CONTENT},
        {"role": "user", "content": USER_CONTENT},
        {
            "role": "assistant",
            "content": "",
            "reasoning_content": "",
            "tool_calls": [{
                "id": CALL_ID,
                "type": "function",
                "function": {"name": "ping", "arguments": "{}"}
            }]
        }
    ])
}

/// 四条请求序列化后，tool 消息只按用例变化。
///
/// **作用**：锁住实调发出去之前的形状。
/// **边界**：不拨号。不检查服务端是否接受。
#[test]
fn pairing_requests_keep_the_declared_tool_shape() {
    let header = json!({
        "model": DEFAULT_MODEL,
        "reasoning_effort": "low",
        "max_tokens": 1,
        "tools": [{
            "type": "function",
            "function": {
                "name": "ping",
                "description": "Reply with ok.",
                "parameters": {"type": "object", "properties": {}}
            }
        }]
    });
    let cases = [
        (
            PairingCase::PairedPlain,
            Some(json!({"role": "tool", "content": PLAIN_CONTENT, "tool_call_id": CALL_ID})),
        ),
        (
            PairingCase::PairedStub,
            Some(json!({"role": "tool", "content": STUB_CONTENT, "tool_call_id": CALL_ID})),
        ),
        (PairingCase::MissingTool, None),
        (
            PairingCase::MismatchedId,
            Some(json!({"role": "tool", "content": PLAIN_CONTENT, "tool_call_id": OTHER_CALL_ID})),
        ),
    ];
    for (case, tool) in cases {
        let value = serde_json::to_value(pairing_request(case)).expect("request encodes");
        assert_eq!(value["model"], header["model"]);
        assert_eq!(value["reasoning_effort"], header["reasoning_effort"]);
        assert_eq!(value["max_tokens"], header["max_tokens"]);
        assert_eq!(value["tools"], header["tools"]);
        let messages = value["messages"].as_array().expect("messages array");
        let shared = shared_messages();
        let shared = shared.as_array().expect("shared messages");
        assert_eq!(&messages[..3], shared.as_slice());
        match tool {
            Some(expected) => {
                assert_eq!(messages.len(), 4);
                assert_eq!(messages[3], expected);
            }
            None => assert_eq!(messages.len(), 3),
        }
    }
}

/// 成对占位是否被接受。默认不跑。
///
/// **作用**：先发对照。对照完成后再发占位、缺消息和错配。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时直接返回，不拨号。对照不是 `completed` 时不再发其余三次。不写 Run、档案或记录文件。stdout 不含正文、摘要和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn paired_placeholder_when_deepseek_client_response() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let control = pairing_label(&mut client, PairingCase::PairedPlain).await;
    print_case(PairingCase::PairedPlain, &control);
    if control != "completed" {
        println!("stopped reason=control_not_completed");
        return;
    }
    for case in [
        PairingCase::PairedStub,
        PairingCase::MissingTool,
        PairingCase::MismatchedId,
    ] {
        let label = pairing_label(&mut client, case).await;
        print_case(case, &label);
    }
}
