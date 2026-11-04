//! T5 中途追加探针。
//!
//! **作用**：在同一段短对话上分别追加一条 system 消息或一个工具声明，记录是否生效和缓存命中。
//! **边界**：默认测试不拨号，不写 Run、事件或档案。只有 `#[ignore]` 的测试连接 DeepSeek。stdout 不含消息正文、思维链、工具参数或密钥。

use fluxaxiom_engine_custom::{
    execute, CancellationToken, ChatCompletion, ChatError, ChatRequest, DeepSeekClient, Message,
    ObservedText, ReasoningEffort, SleepBackoff, TokenCount, ToolCall, ToolSpec, API_KEY_ENV,
    DEFAULT_MODEL,
};
use serde_json::json;

/// 第一条 system 要求的词。
const FIRST_WORD: &str = "red";

/// 后追加的 system 要求的词。
const ADDED_WORD: &str = "blue";

/// 两段请求都声明的工具。
const FIRST_TOOL: &str = "ping";

/// 只在第二次工具请求里追加的工具。
const ADDED_TOOL: &str = "pong";

/// 用来把前缀垫到能观察缓存命中的汉字数。
const FILL_CHARS: usize = 1024;

/// 一次实调留在 stdout 里的用量。
///
/// **作用**：只保留 prompt、命中和未命中。
/// **边界**：未知保持 `unknown`，不用 0 代替。不含正文。
struct UsageLabel {
    /// 服务端 `prompt_tokens`。`unknown` 表示响应没有给出已知数。
    prompt: String,
    /// 服务端 `prompt_cache_hit_tokens`。`unknown` 表示该字段缺失。
    hit: String,
    /// 服务端 `prompt_cache_miss_tokens`。`unknown` 表示该字段缺失。
    miss: String,
}

/// 追加之后能否判断模型跟着新指令走。
///
/// **作用**：stdout 用短标签表示判断结果。
/// **边界**：`Unclear` 表示正文或工具调用对不上两个预定词，不是 HTTP 失败。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Followed {
    /// 正文恰好是新增 system 要求的词，或调用了新增工具。
    Yes,
    /// 正文恰好是旧 system 要求的词，或没有调用新增工具。
    No,
    /// 完成了，但正文对不上两个预定词。
    Unclear,
}

/// 计数在 stdout 里的写法。
///
/// **作用**：已知数照印，缺失印成 `unknown`。
/// **参数**：`tokens` 是用量里的一项。`Unknown` 表示服务端没有给出已知数。
/// **边界**：不把未知印成 0。
fn count_label(tokens: TokenCount) -> String {
    match tokens {
        TokenCount::Known(value) => value.to_string(),
        TokenCount::Unknown => "unknown".to_owned(),
    }
}

/// 失败时打印的状态标签。
///
/// **作用**：stdout 只留类别和状态码。
/// **参数**：`error` 是一次 `execute` 失败。`Rejected` 和 `Unavailable` 带上状态码。
/// **边界**：不打印 `summary`、正文或密钥。
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

/// 把汉字垫在短指令后面。
///
/// **作用**：让前缀长到可能产生缓存命中。
/// **参数**：`instruction` 是放在填充前面的一句短指令。空串时 user 只有填充。
/// **边界**：填充只含「中」。不把这段文字写入 stdout。
fn filled_user(instruction: &str) -> String {
    format!("{instruction}\n{}", "中".repeat(FILL_CHARS))
}

/// 组装一条低思考强度的请求。
///
/// **作用**：system 探针和工具探针共用模型参数。
/// **参数**：`messages` 是这次的消息，按顺序发送。`tools` 非空时带上工具；空列表不写 `tools` 键。
/// **边界**：模型是 `deepseek-flash`。`reasoning_effort` 为 `Low`，`max_tokens` 为 1024。不写缓存断点。不拨号。
fn update_request(messages: Vec<Message>, tools: Vec<ToolSpec>) -> ChatRequest {
    let mut request = ChatRequest::new(DEFAULT_MODEL, messages)
        .with_tools(tools)
        .with_max_tokens(1024)
        .expect("max_tokens 1024 is valid");
    request.reasoning_effort = ReasoningEffort::Low;
    request
}

/// 一个只声明名字的空参数工具。
///
/// **作用**：工具列表里的 `ping` 和 `pong` 都用这个形状。
/// **参数**：`name` 是工具名。空串也会照写进声明。
/// **边界**：描述为空。参数 schema 是没有属性的 object。
fn named_tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

/// system 探针的第一段消息。
///
/// **作用**：给追加 system 的请求当不变的前缀。
/// **边界**：只有一条 system 和一条 user。不包含新增的词。
fn system_base_messages() -> Vec<Message> {
    vec![
        Message::System {
            content: format!("Reply with the single word {FIRST_WORD}."),
        },
        Message::User {
            content: filled_user("Answer now."),
        },
    ]
}

/// 在第一段 system 对话末尾再加一条 system。
///
/// **作用**：这是 system 探针的第二次请求。
/// **边界**：前两条消息与 [`system_base_messages`] 相同。新增的是最后一条。
fn system_appended_messages() -> Vec<Message> {
    let mut messages = system_base_messages();
    messages.push(Message::System {
        content: format!("Reply with the single word {ADDED_WORD}."),
    });
    messages
}

/// 工具探针的消息。两次请求共用。
///
/// **作用**：工具列表变化时，消息保持这一份。
/// **边界**：不声明工具名以外的任务。user 点名新增工具。
fn tool_messages() -> Vec<Message> {
    vec![
        Message::System {
            content: "Call one tool.".to_owned(),
        },
        Message::User {
            content: filled_user(&format!("Call the tool named {ADDED_TOOL}.")),
        },
    ]
}

/// 判断正文是不是某一个预定词。
///
/// **作用**：system 探针把模型正文收成 yes、no 或 unclear。
/// **参数**：`content` 是这次 completion 的正文。`Absent` 和 `Empty` 都算对不上。
/// **边界**：比较前去掉首尾空白并转成小写。不接受词以外的句子。不把正文交回调用方。
fn system_followed(content: &ObservedText) -> Followed {
    let ObservedText::Text(text) = content else {
        return Followed::Unclear;
    };
    let word = text.trim().to_lowercase();
    if word == ADDED_WORD {
        Followed::Yes
    } else if word == FIRST_WORD {
        Followed::No
    } else {
        Followed::Unclear
    }
}

/// 判断这次有没有调用新增工具。
///
/// **作用**：工具探针只看工具名。
/// **参数**：`calls` 是这次响应的工具调用。空切片表示没有调用。
/// **边界**：不看参数。名字等于 [`ADDED_TOOL`] 就是 yes，否则是 no。
fn tool_followed(calls: &[ToolCall]) -> Followed {
    if calls.iter().any(|call| call.name == ADDED_TOOL) {
        Followed::Yes
    } else {
        Followed::No
    }
}

/// 标签在 stdout 里的写法。
///
/// **作用**：避免打印 yes/no 以外的正文。
/// **参数**：`followed` 是 [`system_followed`] 或 [`tool_followed`] 的结果。
/// **边界**：三个变体各对应一个固定词。
fn followed_label(followed: Followed) -> &'static str {
    match followed {
        Followed::Yes => "yes",
        Followed::No => "no",
        Followed::Unclear => "unclear",
    }
}

/// 发送一条请求。
///
/// **作用**：实调用它取用量；system 探针再取正文判断，工具探针再取工具名。
/// **参数**：`client` 是已带密钥的 DeepSeek 客户端，本次调用会使用它。`request` 是要发送的请求。`followed` 从成功的 completion 里作出判断。
/// **边界**：失败只返回状态标签。400 不在这里重试。判断函数不得把正文写入返回值。
async fn send_update(
    client: &mut DeepSeekClient,
    request: &ChatRequest,
    followed: impl Fn(&ChatCompletion) -> Option<Followed>,
) -> Result<(UsageLabel, Option<Followed>), String> {
    match execute(
        client,
        request,
        &mut SleepBackoff,
        &CancellationToken::new(),
    )
    .await
    {
        Ok(completion) => Ok((
            UsageLabel {
                prompt: count_label(completion.usage.prompt_tokens),
                hit: count_label(completion.usage.prompt_cache_hit_tokens),
                miss: count_label(completion.usage.prompt_cache_miss_tokens),
            },
            followed(&completion),
        )),
        Err(error) => Err(http_label(&error)),
    }
}

/// 打印一次探针。
///
/// **作用**：实调的 stdout 只留探针名、步骤、用量和是否生效。
/// **参数**：`probe` 是 `system` 或 `tool`。`step` 是 `base` 或 `appended`。`outcome` 是 [`send_update`] 的结果。
/// **边界**：失败行没有 hit、miss 和 followed。不打印正文。
fn print_step(probe: &str, step: &str, outcome: &Result<(UsageLabel, Option<Followed>), String>) {
    match outcome {
        Ok((usage, followed)) => match followed {
            Some(followed) => println!(
                "probe={probe} step={step} http=completed prompt={} hit={} miss={} followed={}",
                usage.prompt,
                usage.hit,
                usage.miss,
                followed_label(*followed)
            ),
            None => println!(
                "probe={probe} step={step} http=completed prompt={} hit={} miss={}",
                usage.prompt, usage.hit, usage.miss
            ),
        },
        Err(label) => println!("probe={probe} step={step} http={label}"),
    }
}

/// 第二次请求只多出约定的那一处变化。
///
/// **作用**：锁住实调发出去之前的消息和工具列表。
/// **边界**：不拨号。不检查服务端是否接受。
#[test]
fn update_requests_change_only_one_declaration() {
    let system_base = update_request(system_base_messages(), Vec::new());
    let system_added = update_request(system_appended_messages(), Vec::new());
    let tool_base = update_request(tool_messages(), vec![named_tool(FIRST_TOOL)]);
    let tool_added = update_request(
        tool_messages(),
        vec![named_tool(FIRST_TOOL), named_tool(ADDED_TOOL)],
    );
    let system_base = serde_json::to_value(system_base).expect("system base encodes");
    let system_added = serde_json::to_value(system_added).expect("system added encodes");
    let tool_base = serde_json::to_value(tool_base).expect("tool base encodes");
    let tool_added = serde_json::to_value(tool_added).expect("tool added encodes");

    assert!(system_base.get("tools").is_none());
    assert!(system_added.get("tools").is_none());
    let base_messages = system_base["messages"].as_array().expect("base messages");
    let added_messages = system_added["messages"].as_array().expect("added messages");
    assert_eq!(added_messages.len(), base_messages.len() + 1);
    assert_eq!(
        &added_messages[..base_messages.len()],
        base_messages.as_slice()
    );
    assert_eq!(added_messages.last().expect("new system")["role"], "system");

    assert_eq!(tool_base["messages"], tool_added["messages"]);
    let base_tools = tool_base["tools"].as_array().expect("base tools");
    let added_tools = tool_added["tools"].as_array().expect("added tools");
    assert_eq!(added_tools.len(), base_tools.len() + 1);
    assert_eq!(&added_tools[..base_tools.len()], base_tools.as_slice());
    assert_eq!(
        added_tools.last().expect("new tool")["function"]["name"],
        ADDED_TOOL
    );
}

/// 中途追加 system 和工具声明。默认不跑。
///
/// **作用**：先做 system 的两段，再做工具的两段。某一段没有完成就停。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时直接返回，不拨号。不写 Run、档案或记录文件。stdout 不含正文、思维链、工具参数和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn mid_conversation_update_when_deepseek_client_response() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let system_base = update_request(system_base_messages(), Vec::new());
    let base = send_update(&mut client, &system_base, |_| None).await;
    print_step("system", "base", &base);
    if base.is_err() {
        println!("stopped reason=not_completed probe=system step=base");
        return;
    }
    let system_added = update_request(system_appended_messages(), Vec::new());
    let added = send_update(&mut client, &system_added, |completion| {
        Some(system_followed(&completion.content))
    })
    .await;
    print_step("system", "appended", &added);
    if added.is_err() {
        println!("stopped reason=not_completed probe=system step=appended");
        return;
    }

    let tool_base = update_request(tool_messages(), vec![named_tool(FIRST_TOOL)]);
    let base = send_update(&mut client, &tool_base, |_| None).await;
    print_step("tool", "base", &base);
    if base.is_err() {
        println!("stopped reason=not_completed probe=tool step=base");
        return;
    }
    let tool_added = update_request(
        tool_messages(),
        vec![named_tool(FIRST_TOOL), named_tool(ADDED_TOOL)],
    );
    let added = send_update(&mut client, &tool_added, |completion| {
        Some(tool_followed(&completion.tool_calls))
    })
    .await;
    print_step("tool", "appended", &added);
    if added.is_err() {
        println!("stopped reason=not_completed probe=tool step=appended");
    }
}
