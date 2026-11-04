//! T8 探针的离线比例计算。
//!
//! **作用**：把两次 `prompt_tokens` 换成比例，并判断 200 字与 2000 字两档是否稳定。也生成固定类别的探针正文。
//! **边界**：默认测试不发网络请求，不写 Run、事件或档案。文件末尾的 `#[ignore]` 测试只在显式 `--ignored` 时拨号。

use fluxaxiom_contract::TokenCount;
use fluxaxiom_engine_custom::{
    execute, CancellationToken, ChatError, ChatRequest, DeepSeekClient, Message, ReasoningEffort,
    SleepBackoff, API_KEY_ENV, DEFAULT_MODEL,
};

/// 一次探针的比例计算失败。
///
/// **作用**：`probe_ratio` 拒绝无法产生比例的输入。
/// **边界**：只有字数为 0 和 token 差为负两类。不表示 HTTP 失败。
#[derive(Debug, PartialEq, Eq)]
enum ProbeError {
    /// 探针字数为 0，不能做除法。
    EmptyProbe,

    /// 第二次的 prompt token 比第一次少。原始两次计数由调用方另记，这里不产生比例。
    NegativeDelta,
}

/// 200 字与 2000 字两档比例是否稳定。
///
/// **作用**：`two_size_verdict` 的结果。两档原始比例仍由调用方保存。
/// **边界**：`Unknown` 是判定结果，不是错误。不保存合并后的单个比例。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// 相对差不超过 0.10，或两档都是 0。
    Stable,

    /// 相对差大于 0.10。不取两档的平均。
    Unknown,
}

/// 探针正文重复哪一类字符。
///
/// **作用**：把类别固定成英文字母或汉字，调用方不能再传入别的字符。
/// **边界**：只有这两类。重复用的具体字符由 [`probe_text`] 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeClass {
    /// 重复 ASCII 字母 `a`。
    Letter,
    /// 重复汉字「中」。一个「中」是一个 Unicode 标量。
    Han,
}

/// 生成正好 `chars` 个相同字符的探针正文。
///
/// **作用**：供 200 字和 2000 字两档使用。调用方拥有返回的字符串，并可以追加到 user 末尾。
/// **参数**：`class` 选择重复 `a` 还是「中」。`chars` 是 Unicode 标量个数，0 返回空串。
/// **边界**：`Letter` 的结果只含 `a`，`Han` 的结果只含「中」。不发请求，不计算比例。
fn probe_text(class: ProbeClass, chars: usize) -> String {
    let unit = match class {
        ProbeClass::Letter => "a",
        ProbeClass::Han => "中",
    };
    unit.repeat(chars)
}


/// 比较 200 字与 2000 字两档比例的相对差。
///
/// **作用**：决定这一类字符的比例能不能当作稳定结果。
/// **参数**：`r_200` 是 200 字探针的 token 比例，由 `probe_ratio` 返回。`r_2000` 是 2000 字探针的 token 比例，来源相同。
/// **边界**：两档都是 0 时返回 `Stable`，避免除以 0。相对差大于 0.10 返回 `Unknown`，不取平均。
/// 不拒绝负数和非有限数；较大的数为负时相对差为负，大于 0.10 的比较不成立，返回 `Stable`。
fn two_size_verdict(r_200 : f64, r_2000: f64) -> Verdict{
    if r_200 == 0.0 && r_2000 == 0.0 {
        return Verdict::Stable
    }

    let gap = (r_200 - r_2000).abs() / r_200.max(r_2000);

    if gap > 0.10 {
        return Verdict::Unknown;
    }
    return Verdict::Stable
}


/// 用两次 prompt token 的差除以探针字数。
///
/// **作用**：得到某一档探针的 token 比例。
/// **参数**：`prompt_before` 是只含前缀的 `prompt_tokens`，0 允许。`prompt_after` 是前缀末尾追加探针后的 `prompt_tokens`，必须不小于 `prompt_before`。`chars` 是探针的 Unicode 标量数，必须大于 0。
/// **边界**：`chars` 为 0 时返回 `EmptyProbe`。`prompt_after` 更小时返回 `NegativeDelta`，不产生比例。差值用 `checked_sub`。
fn probe_ratio(prompt_before: u64, prompt_after: u64, chars: u64) -> Result<f64, ProbeError> {
    if chars == 0 {
        return Err(ProbeError::EmptyProbe);
    }

    let delta = prompt_after
        .checked_sub(prompt_before)
        .ok_or(ProbeError::NegativeDelta)?;
    Ok(delta as f64 / chars as f64)
}

/// 差为 2、字数为 4 时比例是 0.5。
///
/// **作用**：锁住 `probe_ratio` 使用减法。
/// **边界**：不访问网络。
#[test]
fn normal_sub() {
    assert_eq!(probe_ratio(10, 12, 4), Ok(0.5))
}

/// 字数为 0 时不能做除法。
///
/// **作用**：锁住 `EmptyProbe`。
/// **边界**：token 数合法也不产生比例。
#[test]
fn assert_chars_eq_0(){
    assert_eq!(probe_ratio(10, 12, 0), Err(ProbeError::EmptyProbe))
}

/// 第二次 token 更少时拒绝产生比例。
///
/// **作用**：锁住 `NegativeDelta`。
/// **边界**：不把负差编成 0。
#[test]
fn assert_negative_delta(){
    assert_eq!(probe_ratio(12, 10, 4), Err(ProbeError::NegativeDelta))
}

/// 字母探针的字数等于参数，并且不含数字。
///
/// **作用**：锁住 `Letter` 重复的是 `a`。
/// **边界**：不访问网络。2000 字与 200 字使用同一个生成规则。
#[test]
fn letter_probe_has_exact_length_and_no_digits() {
    let text = probe_text(ProbeClass::Letter, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == 'a'));
    assert_eq!(probe_text(ProbeClass::Letter, 2000).chars().count(), 2000);
}

/// 汉字探针的字数等于参数。
///
/// **作用**：锁住 `Han` 重复的是「中」，每个「中」只计一个 Unicode 标量。
/// **边界**：不访问网络。0 返回空串。
#[test]
fn han_probe_counts_one_scalar_per_character() {
    let text = probe_text(ProbeClass::Han, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == '中'));
    assert_eq!(probe_text(ProbeClass::Han, 0).chars().count(), 0);
}

/// 四组两档比例覆盖稳定、全 0、门槛和超门槛。
///
/// **作用**：锁住 `two_size_verdict` 的相对差规则。
/// **边界**：`0.9` 与 `1.0` 的相对差是 0.10，仍为 `Stable`。`0.8` 与 `1.0` 为 `Unknown`。
#[test]
fn assert_two_size_verdict(){
    assert_eq!(two_size_verdict(0.5, 0.5), Verdict::Stable);
    assert_eq!(two_size_verdict(0.0, 0.0), Verdict::Stable);
    assert_eq!(two_size_verdict(0.9, 1.0), Verdict::Stable);
    assert_eq!(two_size_verdict(0.8, 1.0), Verdict::Unknown);
}

/// 实调前缀的 system。四次探针共用，不追加探针。
const LIVE_PREFIX_SYSTEM: &str = "reply in one word";

/// 实调前缀的 user。探针正文只追加在这条字符串后面。
const LIVE_PREFIX_USER: &str = "prefix";

/// 一次实调没有拿到可用的 `prompt_tokens`。
///
/// **作用**：把用量缺失和传输失败分开。
/// **边界**：不保存请求正文、思维链或密钥。
enum LivePromptError {
    /// 响应里没有已知的 `prompt_tokens`。不是 HTTP 状态。
    UsageMissing,
    /// 传输层失败。打印时只用类别，不用 `Display`，避免带上服务端摘要。
    Chat(ChatError),
}

/// 组装一条不带工具的实调请求。
///
/// **作用**：前缀请求和追加探针的请求共用同一套模型参数。
/// **参数**：`user` 是这次的 user 正文。前缀调用只含 [`LIVE_PREFIX_USER`]；探针调用在它后面接上探针。
/// **边界**：`max_tokens` 固定为 1，`reasoning_effort` 固定为 `Low`。不写入 `tools`。
fn live_request(user: String) -> ChatRequest {
    let mut request = ChatRequest::new(
        DEFAULT_MODEL,
        vec![
            Message::System {
                content: LIVE_PREFIX_SYSTEM.to_owned(),
            },
            Message::User { content: user },
        ],
    )
    .with_max_tokens(1)
    .expect("max_tokens 1 is valid");
    request.reasoning_effort = ReasoningEffort::Low;
    request
}

/// 发送一条 user 文本，取出服务端的 `prompt_tokens`。
///
/// **作用**：前缀和每一档探针都走这里。
/// **参数**：`client` 是已带密钥的 DeepSeek 客户端，本次调用会使用它。`user` 是完整的 user 正文，不写入输出。
/// **边界**：成功时传输层不把 HTTP 200 交出来。用量缺失返回 `UsageMissing`，不编成 0。
async fn live_prompt(client: &mut DeepSeekClient, user: &str) -> Result<u64, LivePromptError> {
    let request = live_request(user.to_owned());
    let completion = execute(
        client,
        &request,
        &mut SleepBackoff,
        &CancellationToken::new(),
    )
    .await
    .map_err(LivePromptError::Chat)?;
    match completion.usage.prompt_tokens {
        TokenCount::Known(tokens) => Ok(tokens),
        TokenCount::Unknown => Err(LivePromptError::UsageMissing),
    }
}

/// 失败时打印的状态标签。
///
/// **作用**：stdout 只留类别和状态码。
/// **参数**：`error` 是一次实调失败。
/// **边界**：不打印 `summary`、正文或密钥。成功路径不使用这个函数。
fn live_http_label(error: &LivePromptError) -> String {
    match error {
        LivePromptError::UsageMissing => "usage_missing".to_owned(),
        LivePromptError::Chat(ChatError::Rejected { status, .. }) => format!("rejected:{status}"),
        LivePromptError::Chat(ChatError::Unavailable { status, .. }) => {
            format!("unavailable:{status}")
        }
        LivePromptError::Chat(ChatError::StreamInterrupted) => "stream_interrupted".to_owned(),
        LivePromptError::Chat(ChatError::Cancelled) => "cancelled".to_owned(),
        LivePromptError::Chat(ChatError::InvalidToolArguments { .. }) => {
            "invalid_tool_arguments".to_owned()
        }
        LivePromptError::Chat(ChatError::MalformedToolCall { .. }) => {
            "malformed_tool_call".to_owned()
        }
        LivePromptError::Chat(ChatError::NotEncodable) => "not_encodable".to_owned(),
        LivePromptError::Chat(ChatError::MissingCredential) => "missing_credential".to_owned(),
        LivePromptError::Chat(ChatError::Transport) => "transport".to_owned(),
    }
}

/// 打印一档探针的比例，或失败类别。
///
/// **作用**：把 `probe_ratio` 用到实调得到的两个 token 数上。
/// **参数**：`class` 是 `letter` 或 `han`。`chars` 是探针字数。`before` 是前缀的 `prompt_tokens`。`after` 是追加探针后的结果。
/// **边界**：不打印探针正文。成功时 HTTP 状态写成 `completed`，因为客户端不返回 200。
fn print_probe(class: &str, chars: usize, before: u64, after: Result<u64, LivePromptError>) {
    match after {
        Ok(tokens) => match probe_ratio(before, tokens, chars as u64) {
            Ok(ratio) => println!(
                "{class} chars={chars} prompt_before={before} prompt_after={tokens} ratio={ratio} http=completed"
            ),
            Err(error) => println!(
                "{class} chars={chars} prompt_before={before} prompt_after={tokens} ratio={error:?} http=completed"
            ),
        },
        Err(error) => println!(
            "{class} chars={chars} prompt_before={before} http={}",
            live_http_label(&error)
        ),
    }
}

/// 汉字和英文字母各两档的实调。默认不跑。
///
/// **作用**：用同一条前缀测量 200 字和 2000 字探针的 `prompt_tokens`。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时直接返回，不拨号。不写 Run、档案或记录文件。stdout 不含探针正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_completion_when_deepseek_client_response() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        return;
    }

    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let prompt_before = match live_prompt(&mut client, LIVE_PREFIX_USER).await {
        Ok(tokens) => {
            println!("prefix prompt_tokens={tokens} http=completed");
            tokens
        }
        Err(error) => {
            println!("prefix http={}", live_http_label(&error));
            return;
        }
    };

    let probes = [
        ("letter", 200, probe_text(ProbeClass::Letter, 200)),
        ("letter", 2000, probe_text(ProbeClass::Letter, 2000)),
        ("han", 200, probe_text(ProbeClass::Han, 200)),
        ("han", 2000, probe_text(ProbeClass::Han, 2000)),
    ];
    for (class, chars, probe) in probes {
        let user = format!("{LIVE_PREFIX_USER}{probe}");
        print_probe(
            class,
            chars,
            prompt_before,
            live_prompt(&mut client, &user).await,
        );
    }
}