//! T3 缓存探针。
//!
//! **作用**：用同一条短前缀测量 1 分钟和 5 分钟后的缓存命中，再截断这条前缀看命中落在哪一档。
//! **边界**：默认测试不拨号，不写 Run、事件或档案。15 分钟和 60 分钟不测。只有 `#[ignore]` 的测试连接 DeepSeek。stdout 不含前缀正文、错误摘要或密钥。

use std::time::{Duration, Instant};

use fluxaxiom_engine_custom::{
    execute, CancellationToken, ChatError, ChatRequest, DeepSeekClient, Message, ReasoningEffort,
    SleepBackoff, TokenCount, API_KEY_ENV, DEFAULT_MODEL,
};
/// 完整前缀的汉字数。汉字比例按 1 计，用来凑出足以观察命中的长度。
const FULL_CHARS: usize = 4096;

/// 截断档的汉字数。每一档都是完整前缀的真前缀。
const TRUNC_CHARS: [usize; 3] = [1024, 2048, 3072];

/// 从第一次发送起算的重发间隔，单位秒。只有 1 分钟和 5 分钟。
const DELAYS_SECS: [u64; 2] = [60, 300];

/// 四条请求共用的 system 正文。
const SYSTEM_CONTENT: &str = "reply in one word";

/// 一次实调打印出来的用量。
///
/// **作用**：stdout 只留 prompt、命中和未命中三个计数。
/// **边界**：未知保持 `unknown`，不用 0 代替。不含正文。
struct UsageLabel {
    /// 服务端 `prompt_tokens`。`unknown` 表示响应里没有已知数。
    prompt: String,
    /// 服务端 `prompt_cache_hit_tokens`。`unknown` 表示该字段缺失。
    hit: String,
    /// 服务端 `prompt_cache_miss_tokens`。`unknown` 表示该字段缺失。
    miss: String,
}

/// 把汉字重复成 user 正文。
///
/// **作用**：完整前缀和截断档共用同一种填充。
/// **参数**：`chars` 是「中」的重复次数。0 得到空串。
/// **边界**：不插入其他字符。不把正文写入 stdout。
fn prefix_user(chars: usize) -> String {
    "中".repeat(chars)
}

/// 组装一条不带工具的缓存探针。
///
/// **作用**：间隔重发和截断档都从这里拿请求。
/// **参数**：`chars` 是 user 里「中」的个数。完整前缀用 [`FULL_CHARS`]，截断档必须更短。
/// **边界**：模型是 `deepseek-flash`。`reasoning_effort` 为 `Low`，`max_tokens` 为 1。不写 `tools`，也不写缓存断点字段。不拨号。
fn cache_request(chars: usize) -> ChatRequest {
    let mut request = ChatRequest::new(
        DEFAULT_MODEL,
        vec![
            Message::System {
                content: SYSTEM_CONTENT.to_owned(),
            },
            Message::User {
                content: prefix_user(chars),
            },
        ],
    )
    .with_max_tokens(1)
    .expect("max_tokens 1 is valid");
    request.reasoning_effort = ReasoningEffort::Low;
    request
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

/// 发送一条探针。
///
/// **作用**：实调用它取用量，失败时只留下状态标签。
/// **参数**：`client` 是已带密钥的 DeepSeek 客户端，本次调用会使用它。`chars` 是这次 user 里「中」的个数。
/// **边界**：成功时不看结束原因。用量缺失保持未知。400 不在这里重试。
async fn cache_outcome(client: &mut DeepSeekClient, chars: usize) -> Result<UsageLabel, String> {
    let request = cache_request(chars);
    match execute(
        client,
        &request,
        &mut SleepBackoff,
        &CancellationToken::new(),
    )
    .await
    {
        Ok(completion) => Ok(UsageLabel {
            prompt: count_label(completion.usage.prompt_tokens),
            hit: count_label(completion.usage.prompt_cache_hit_tokens),
            miss: count_label(completion.usage.prompt_cache_miss_tokens),
        }),
        Err(error) => Err(http_label(&error)),
    }
}

/// 打印一次间隔重发。
///
/// **作用**：实调按延迟记录完整前缀的命中。
/// **参数**：`delay_secs` 是距第一次发送的秒数，0 表示第一次。`outcome` 是 [`cache_outcome`] 的结果。
/// **边界**：不打印前缀正文。失败行没有 hit 和 miss。
fn print_delay(delay_secs: u64, outcome: &Result<UsageLabel, String>) {
    match outcome {
        Ok(usage) => println!(
            "delay_s={delay_secs} chars={FULL_CHARS} http=completed prompt={} hit={} miss={}",
            usage.prompt, usage.hit, usage.miss
        ),
        Err(label) => println!("delay_s={delay_secs} chars={FULL_CHARS} http={label}"),
    }
}

/// 打印一档截断。
///
/// **作用**：实调记录较短前缀的命中。
/// **参数**：`chars` 是这一档的汉字数，必须小于 [`FULL_CHARS`]。`outcome` 是这一档的结果。
/// **边界**：不打印前缀正文。失败行没有 hit 和 miss。
fn print_trunc(chars: usize, outcome: &Result<UsageLabel, String>) {
    match outcome {
        Ok(usage) => println!(
            "trunc_chars={chars} http=completed prompt={} hit={} miss={}",
            usage.prompt, usage.hit, usage.miss
        ),
        Err(label) => println!("trunc_chars={chars} http={label}"),
    }
}

/// 请求不带工具和缓存断点，截断是完整前缀的真前缀。
///
/// **作用**：锁住实调发出去之前的形状，并确认这次不测 15 分钟和 60 分钟。
/// **边界**：不拨号，也不睡眠。
#[test]
fn cache_probe_shape_uses_only_one_and_five_minutes() {
    assert_eq!(DELAYS_SECS, [60, 300]);
    let full = cache_request(FULL_CHARS);
    let value = serde_json::to_value(&full).expect("request encodes");
    let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
    assert_eq!(
        keys,
        ["max_tokens", "messages", "model", "reasoning_effort"]
    );
    assert_eq!(value["model"], DEFAULT_MODEL);
    assert_eq!(value["reasoning_effort"], "low");
    assert_eq!(value["max_tokens"], 1);
    assert!(value.get("tools").is_none());
    let full_user = prefix_user(FULL_CHARS);
    assert_eq!(full_user.chars().count(), FULL_CHARS);
    for chars in TRUNC_CHARS {
        assert!(chars < FULL_CHARS);
        assert!(full_user.starts_with(&prefix_user(chars)));
        let short = serde_json::to_value(cache_request(chars)).expect("trunc encodes");
        assert!(short.get("tools").is_none());
        assert_eq!(short["messages"][0]["content"], SYSTEM_CONTENT);
    }
}

/// 1 分钟和 5 分钟的缓存命中，以及三档截断。默认不跑。
///
/// **作用**：先发完整前缀，再在 1 分钟和 5 分钟重发同一条。两档都完成后才截断。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时直接返回，不拨号。某一档没有 `completed` 时停在该档。不写 Run、档案或记录文件。stdout 不含正文、摘要和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set; waits five minutes"]
async fn cache_hit_after_one_and_five_minutes() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let started = Instant::now();
    let first = cache_outcome(&mut client, FULL_CHARS).await;
    print_delay(0, &first);
    if first.is_err() {
        println!("stopped reason=delay_not_completed delay_s=0");
        return;
    }
    for delay_secs in DELAYS_SECS {
        let elapsed = started.elapsed();
        let wait = Duration::from_secs(delay_secs).saturating_sub(elapsed);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let outcome = cache_outcome(&mut client, FULL_CHARS).await;
        print_delay(delay_secs, &outcome);
        if outcome.is_err() {
            println!("stopped reason=delay_not_completed delay_s={delay_secs}");
            return;
        }
    }
    for chars in TRUNC_CHARS {
        let outcome = cache_outcome(&mut client, chars).await;
        print_trunc(chars, &outcome);
        if outcome.is_err() {
            println!("stopped reason=trunc_not_completed trunc_chars={chars}");
            return;
        }
    }
}
