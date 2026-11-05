//! T0 未填充基线，以及 T7、T9 和 L2 遮蔽后的采样。
//!
//! **作用**：测量 `mini-inventory` 第 1 次和第 5 次模型请求的自一致率。T7 只把第 5 次请求里 assistant 的 `reasoning_content` 改成空字符串。T9 只在第 5 次请求的第一条 user 末尾声明剩余空间远小于窗口。L2 测量把保护窗口外的工具正文换成存根。
//! **边界**：不接 Engine，不写 Run、事件或档案。默认不拨号。stdout 不含 prompt、思维链、工具正文或密钥。

use std::path::PathBuf;

use fluxaxiom_engine_custom::{
    decision_of, mode_match_variance, pairwise_agreement, CancellationToken, ChatCompletion,
    ChatTransport, Decision, DeepSeekClient, FinishReason, ObservedText, Round, TokenCount,
    API_KEY_ENV,
};
use serde_json::Value;

/// 第 1 次请求的 blob。来自 2026-10-26 的 Run。
const REQUEST_K1_SHA256: &str = "f48efa04c0446c5ecae4b5d9de2397f5c3cbb93430c184d975f69e88dd11ad97";

/// 第 5 次请求的 blob。来自同一次 Run。当时的下一步是一次 `read_file`。
const REQUEST_K5_SHA256: &str = "005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc";

/// 同一条完整请求的采样次数。
const SAMPLES: usize = 5;

/// 最近多少个模型轮的工具结果保持原文。与上下文模块的保护窗口相同。
const PROTECT_ROUNDS: usize = 2;

/// 16K 档加在第一条 user 末尾的汉字数。未填充的第 5 次请求约 2964 token，汉字比例按 1 计。
const FILL_HAN_CHARS: usize = 13_000;

/// 打印一条签名。
///
/// **作用**：采样输出只留比较用的字段。
/// **参数**：`index` 从 1 起。`decision` 是这次采样的下一步。
/// **边界**：不打印正文。
fn print_decision(index: usize, decision: &Decision) {
    let tools: Vec<String> = decision
        .tools
        .iter()
        .map(|(name, arguments)| format!("{name} {arguments}"))
        .collect();
    println!(
        "sample={index} ended={} tools=[{}] paths=[{}]",
        decision.ended,
        tools.join("; "),
        decision.paths.join(", ")
    );
}

/// 在第一条 user 末尾追加文本。
///
/// **作用**：填充时不改后面的工具结果。
/// **参数**：`request` 是已保存的 Chat Completions 请求。`extra` 是追加到第一条 user 末尾的正文，空串不改变它。
/// **边界**：没有 user 消息或正文不是字符串时返回 `None`。不打印正文。
fn append_text_to_first_user(mut request: Value, extra: &str) -> Option<Value> {
    let messages = request.get_mut("messages")?.as_array_mut()?;
    let user = messages
        .iter_mut()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))?;
    let content = user.get("content")?.as_str()?.to_owned();
    user["content"] = Value::String(format!("{content}{extra}"));
    Some(request)
}

/// 在第一条 user 末尾重复汉字「中」。
///
/// **作用**：把第 5 次请求填充到约 16K，后面的工具结果保持原样。
/// **参数**：`request` 是已保存的 Chat Completions 请求。`chars` 是追加的 Unicode 标量数，0 不改变正文。
/// **边界**：没有 user 消息或正文不是字符串时返回 `None`。不打印正文。
fn append_han_to_first_user(request: Value, chars: usize) -> Option<Value> {
    append_text_to_first_user(request, &"中".repeat(chars))
}

/// 相似干扰的一行。只含邻近路径，不含 `tests/inventory.rs`。
const DECOY_LINE: &str = "src/inventory.rs src/storage.rs src/lib.rs src/main.rs README.md\n";

/// 把相似干扰重复 `repeats` 次。
///
/// **作用**：凑出约 16K 的干扰正文。
/// **参数**：`repeats` 是 [`DECOY_LINE`] 的重复次数，0 返回空串。
/// **边界**：不包含正确下一步的路径 `tests/inventory.rs`。
fn decoy_fill(repeats: usize) -> String {
    DECOY_LINE.repeat(repeats)
}

/// 把已有 assistant 消息的思维链改成空字符串。
///
/// **作用**：T7 发送前只改这一处。
/// **参数**：`request` 是已保存的 Chat Completions 请求。没有 `messages` 数组时返回 `None`。
/// **边界**：键保留，值一律变成 `""`。不改 `content` 和 `tool_calls`。不打印原来的思维链。返回的个数是 assistant 消息条数。
fn blank_assistant_reasoning(mut request: Value) -> Option<(Value, usize)> {
    let messages = request.get_mut("messages")?.as_array_mut()?;
    let mut count = 0usize;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        message["reasoning_content"] = Value::String(String::new());
        count += 1;
    }
    Some((request, count))
}

/// 第 5 次请求未填充基线的下一步。
///
/// **作用**：T7 的 5 次采样和它比较。
/// **边界**：结束为假。工具是一次 `read_file`，路径是 `tests/inventory.rs`。不含思维链。
fn k5_unfilled_baseline() -> Decision {
    Decision {
        ended: false,
        tools: vec![(
            "read_file".to_owned(),
            "{path:\"tests/inventory.rs\",start_line:1}".to_owned(),
        )],
        paths: vec!["tests/inventory.rs".to_owned()],
    }
}

/// 加在第一条 user 末尾的一句剩余空间说明。
const REMAINING_NOTICE: &str = "\n剩余空间远小于上下文窗口。";

/// 正文或思维链是否提到剩余空间。
///
/// **作用**：T9 只把判断写成 yes 或 no。
/// **参数**：`completion` 是一次成功响应。`content` 和 `reasoning` 都检查。
/// **边界**：出现「剩余空间」或 “remaining context” 才算提到。不返回原文。
fn mentions_remaining_space(completion: &ChatCompletion) -> bool {
    observed_mentions(&completion.content) || observed_mentions(&completion.reasoning)
}

/// 一段观测文本是否提到剩余空间。
///
/// **作用**：[`mentions_remaining_space`] 对正文和思维链各查一次。
/// **参数**：`text` 是响应里的一段文本。`Absent` 和 `Empty` 不算提到。
/// **边界**：英文按小写比较。不把这段文本交回调用方。
fn observed_mentions(text: &ObservedText) -> bool {
    let ObservedText::Text(text) = text else {
        return false;
    };
    let lower = text.to_lowercase();
    lower.contains("剩余空间") || lower.contains("remaining context")
}

/// 本机数据目录里的一条已保存请求。
///
/// **作用**：找到要复放的 blob。
/// **参数**：`sha256` 是请求 blob 的十六进制，64 位，不含 `sha256:` 前缀。短于 4 位时返回 `None`。
/// **边界**：文件不在时返回 `None`。不创建目录，不读数据库。
fn stored_request(sha256: &str) -> Option<Value> {
    if sha256.len() < 4 {
        return None;
    }
    let root = std::env::var_os("LOCALAPPDATA")?;
    let mut path = PathBuf::from(root);
    path.push("FluxAxiom");
    path.push("blobs");
    path.push(&sha256[..2]);
    path.push(&sha256[2..4]);
    path.push(sha256);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 对一条请求采样 5 次并打印自一致率。
///
/// **作用**：未填充请求和 16K 填充请求共用采样和签名规则。
/// **参数**：`request` 是已组装好的 Chat Completions 请求。没有密钥时只打印跳过原因。
/// **边界**：不写 Run。失败只打印类别，不打印响应摘要。某一次失败时不计算一致率。
async fn sample_request(request: &Value) {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let cancel = CancellationToken::new();
    let mut decisions = Vec::new();
    for index in 1..=SAMPLES {
        match client.round(request, &cancel).await {
            Ok(Round::Completed(completion)) => {
                println!(
                    "sample={index} finish={} prompt_tokens={}",
                    finish_label(&completion.finish_reason),
                    prompt_label(completion.usage.prompt_tokens)
                );
                let decision = decision_of(&completion);
                print_decision(index, &decision);
                decisions.push(decision);
            }
            Ok(Round::Status { status, .. }) => {
                println!("sample={index} http={status}");
                return;
            }
            Ok(Round::Interrupted) => {
                println!("sample={index} http=interrupted");
                return;
            }
            Ok(Round::Cancelled) => {
                println!("sample={index} http=cancelled");
                return;
            }
            Ok(Round::InvalidToolArguments { .. }) => {
                println!("sample={index} http=invalid_tool_arguments");
                return;
            }
            Ok(Round::MalformedToolCall { .. }) => {
                println!("sample={index} http=malformed_tool_call");
                return;
            }
            Err(_) => {
                println!("sample={index} http=error");
                return;
            }
        }
    }
    let rate = pairwise_agreement(&decisions).expect("five samples");
    let variance = mode_match_variance(&decisions).expect("five samples");
    let mut distinct = decisions.clone();
    distinct.sort();
    distinct.dedup();
    println!(
        "n={SAMPLES} pairs={} agree_rate={rate} variance={variance} distinct={}",
        SAMPLES * (SAMPLES - 1) / 2,
        distinct.len()
    );
}

/// 对一条已保存请求采样 5 次并打印自一致率。
///
/// **作用**：第 1 次和第 5 次请求共用采样和签名规则。
/// **参数**：`sha256` 选择本机 blob。没有密钥或不存在时只打印跳过原因。
/// **边界**：不写 Run。失败只打印类别，不打印响应摘要。某一次失败时不计算一致率。
async fn sample_stored_request(sha256: &str) {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(sha256) else {
        println!("skipped reason=missing_request");
        return;
    };
    sample_request(&request).await;
}

/// 结束原因的短名字。
///
/// **作用**：stdout 不带未知原因的原文。
/// **参数**：`reason` 是这次响应的结束原因。
/// **边界**：未知原因一律写成 `other`。
fn finish_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Other(_) => "other",
    }
}

/// 用量里的 prompt token 短标签。
///
/// **作用**：采样输出只留服务端计数。
/// **参数**：`tokens` 是这次响应的 `prompt_tokens`。`Unknown` 表示响应没有给出已知数。
/// **边界**：不打印请求正文。
fn prompt_label(tokens: TokenCount) -> String {
    match tokens {
        TokenCount::Known(value) => value.to_string(),
        TokenCount::Unknown => "unknown".to_owned(),
    }
}

/// 一次遮蔽改了多少条工具结果。
struct MaskStats {
    /// 请求里的 assistant 消息条数，也就是已完成的模型轮数。
    rounds: usize,
    /// 换成存根的工具消息条数。0 表示没有窗口外的可遮蔽结果。
    stubbed: usize,
    /// 被换掉的原正文 UTF-8 字节数。不含存根本身。
    bytes_before: usize,
}

/// 从一条 assistant 的工具调用里取出名字和参数。
///
/// **作用**：存根要写工具名和参数，不写工具正文。
/// **参数**：`assistant` 是带 `tool_calls` 的消息。`call_id` 是 tool 消息的 `tool_call_id`，空串对不上。
/// **边界**：只认 OpenAI 嵌套的 `function`。对不上时返回 `None`。
fn tool_name_and_arguments(assistant: &Value, call_id: &str) -> Option<(String, String)> {
    if call_id.is_empty() {
        return None;
    }
    let calls = assistant.get("tool_calls")?.as_array()?;
    for call in calls {
        if call.get("id").and_then(Value::as_str) != Some(call_id) {
            continue;
        }
        let function = call.get("function")?;
        let name = function.get("name").and_then(Value::as_str)?.to_owned();
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return Some((name, arguments));
    }
    None
}

/// 把保护窗口之外的工具正文换成 L2 存根。
///
/// **作用**：构造第 5 次请求的遮蔽版。最近 [`PROTECT_ROUNDS`] 个 assistant 轮的工具结果不动。
/// **参数**：`request` 是已保存的 Chat Completions 请求。没有 `messages` 时返回 `None`。
/// **边界**：不改 system、user、assistant。已经是存根的正文跳过。对不上调用的工具消息保持原文，不计入 `stubbed`。
fn mask_old_tool_results(mut request: Value) -> Option<(Value, MaskStats)> {
    let messages = request.get_mut("messages")?.as_array_mut()?;
    let rounds = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .count();
    let mut current_round = 0usize;
    let mut last_assistant = None;
    let mut pending = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" {
            current_round += 1;
            last_assistant = Some(index);
            continue;
        }
        if role != "tool" || current_round == 0 || current_round > rounds.saturating_sub(PROTECT_ROUNDS)
        {
            continue;
        }
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        if content.starts_with("[stale:")
            || content.starts_with("[unchanged:")
            || content.starts_with("[superseded:")
        {
            continue;
        }
        let call_id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(assistant) = last_assistant.and_then(|slot| messages.get(slot)) else {
            continue;
        };
        let Some((name, arguments)) = tool_name_and_arguments(assistant, call_id) else {
            continue;
        };
        let bytes = content.len();
        pending.push((
            index,
            format!(
                "[stale: {name} {arguments}, {bytes} B, sha256:{call_id}; call this tool again if needed]"
            ),
            bytes,
        ));
    }
    let stats = MaskStats {
        rounds,
        stubbed: pending.len(),
        bytes_before: pending.iter().map(|(_, _, bytes)| bytes).sum(),
    };
    for (index, stub, _) in pending {
        messages[index]["content"] = Value::String(stub);
    }
    Some((request, stats))
}

/// 五次相同签名的两两一致率是 1，方差是 0。
#[test]
fn identical_samples_agree_with_zero_variance() {
    let decision = Decision {
        ended: false,
        tools: vec![("list_files".to_owned(), "{path:\".\"}".to_owned())],
        paths: vec![".".to_owned()],
    };
    let samples = vec![decision; SAMPLES];
    assert_eq!(pairwise_agreement(&samples), Some(1.0));
    assert_eq!(mode_match_variance(&samples), Some(0.0));
}

/// 十对里只有一对相同，一致率是 0.1。
#[test]
fn one_agreeing_pair_has_the_pair_rate() {
    let same = Decision {
        ended: false,
        tools: vec![("list_files".to_owned(), "{}".to_owned())],
        paths: Vec::new(),
    };
    let mut samples = vec![same; 2];
    for index in 0..3 {
        samples.push(Decision {
            ended: true,
            tools: vec![(format!("other{index}"), "{}".to_owned())],
            paths: Vec::new(),
        });
    }
    let rate = pairwise_agreement(&samples).expect("five samples");
    assert!((rate - 0.1).abs() < 1e-12);
}

/// 第 1 次请求的未填充完整对完整基线。默认不跑。
///
/// **作用**：同一条已保存请求采样 5 次，打印签名、一致率和方差。
/// **边界**：没有密钥、没有本机 blob 时直接返回。不写 Run。失败只打印类别，不打印响应摘要。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn unfilled_full_baseline_for_mini_inventory_k1() {
    sample_stored_request(REQUEST_K1_SHA256).await;
}

/// 清空思维链只改 assistant 的 `reasoning_content`。
///
/// **作用**：确认键还在、其他字段不动，且旧思维链不再出现在请求里。
/// **边界**：不拨号。夹具里的思维链只用于断言它已被替换。
#[test]
fn blank_reasoning_keeps_the_key_and_other_fields() {
    let request = serde_json::json!({
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {
                "role": "assistant",
                "content": "a",
                "reasoning_content": "secret-chain",
                "tool_calls": [{
                    "id": "c",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            },
            {"role": "tool", "tool_call_id": "c", "content": "body"},
            {"role": "assistant", "content": ""}
        ]
    });
    let (cleared, count) = blank_assistant_reasoning(request).expect("messages");
    assert_eq!(count, 2);
    assert_eq!(cleared["messages"][2]["reasoning_content"], "");
    assert_eq!(cleared["messages"][2]["content"], "a");
    assert_eq!(cleared["messages"][2]["tool_calls"][0]["id"], "c");
    assert_eq!(cleared["messages"][3]["content"], "body");
    assert_eq!(cleared["messages"][4]["reasoning_content"], "");
    assert!(!cleared.to_string().contains("secret-chain"));
    assert_eq!(
        k5_unfilled_baseline().tools[0].1,
        r#"{path:"tests/inventory.rs",start_line:1}"#
    );
}

/// 保护窗口外的工具正文换成存根，最近两轮和 assistant 保持原文。
///
/// **作用**：确认遮蔽只动窗口外的 tool 消息。
/// **边界**：不拨号。哨兵正文只用于断言它已被替换。
#[test]
fn mask_replaces_only_tool_results_outside_the_protect_window() {
    let request = serde_json::json!({
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            assistant_with("c1"),
            {"role": "tool", "tool_call_id": "c1", "content": "SECRET_EARLY"},
            assistant_with("c2"),
            {"role": "tool", "tool_call_id": "c2", "content": "SECRET_MIDDLE"},
            assistant_with("c3"),
            {"role": "tool", "tool_call_id": "c3", "content": "KEEP_NEAR"},
            assistant_with("c4"),
            {"role": "tool", "tool_call_id": "c4", "content": "KEEP_LAST"}
        ]
    });
    let (masked, stats) = mask_old_tool_results(request).expect("messages");
    assert_eq!(stats.rounds, 4);
    assert_eq!(stats.stubbed, 2);
    assert_eq!(stats.bytes_before, "SECRET_EARLY".len() + "SECRET_MIDDLE".len());
    let messages = masked.get("messages").expect("messages");
    assert!(messages[3]["content"].as_str().unwrap().starts_with("[stale:"));
    assert!(messages[5]["content"].as_str().unwrap().starts_with("[stale:"));
    assert_eq!(messages[7]["content"], "KEEP_NEAR");
    assert_eq!(messages[9]["content"], "KEEP_LAST");
    assert_eq!(messages[2]["content"], "keep");
    assert!(!masked.to_string().contains("SECRET_EARLY"));
    assert!(!masked.to_string().contains("SECRET_MIDDLE"));
}

/// 造一条只含一次 `read_file` 的 assistant。
///
/// **作用**：遮蔽单测用它给后面的 tool 消息配对。
/// **参数**：`id` 写入 tool_call id，必须和对应 tool 消息的 `tool_call_id` 相同。
/// **边界**：参数固定为 `{}`。正文固定为 `keep`。
fn assistant_with(id: &str) -> Value {
    serde_json::json!({
        "role": "assistant",
        "content": "keep",
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]
    })
}

/// 把保护窗口外的工具结果换成存根后，比较下一步并记录缓存。默认不跑。
///
/// **作用**：先发两次完整请求，记下触发前的缓存。再对遮蔽版采样 5 次，和未填充基线比较。
/// **边界**：这条请求约 3000 token，低于 128K，`prepare` 不会真的触发 L2。这里按规则强制遮蔽窗口外的工具结果。没有密钥、没有 blob、或没有可遮蔽的工具时直接返回。第一次没有完成就停。stdout 不含工具正文。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn masked_tools_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    let Some((masked, stats)) = mask_old_tool_results(request.clone()) else {
        println!("skipped reason=missing_messages");
        return;
    };
    println!(
        "rounds={} tool_stubbed={} bytes_before={}",
        stats.rounds, stats.stubbed, stats.bytes_before
    );
    if stats.stubbed == 0 {
        println!("stopped reason=nothing_to_mask");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let cancel = CancellationToken::new();
    for index in 1..=2 {
        if !print_usage(&mut client, &request, "before", index, &cancel).await {
            println!("stopped reason=not_completed phase=before sample={index}");
            return;
        }
    }
    let baseline = k5_unfilled_baseline();
    let mut decisions = Vec::new();
    let mut matched = 0usize;
    for index in 1..=SAMPLES {
        match client.round(&masked, &cancel).await {
            Ok(Round::Completed(completion)) => {
                let decision = decision_of(&completion);
                let same = decision == baseline;
                if same {
                    matched += 1;
                }
                println!(
                    "phase=masked sample={index} finish={} prompt_tokens={} hit={} miss={} match_baseline={}",
                    finish_label(&completion.finish_reason),
                    prompt_label(completion.usage.prompt_tokens),
                    prompt_label(completion.usage.prompt_cache_hit_tokens),
                    prompt_label(completion.usage.prompt_cache_miss_tokens),
                    if same { "yes" } else { "no" }
                );
                print_decision(index, &decision);
                decisions.push(decision);
            }
            other => {
                println!(
                    "phase=masked sample={index} http={}",
                    round_label(&other)
                );
                println!("stopped reason=not_completed phase=masked sample={index}");
                return;
            }
        }
    }
    let rate = pairwise_agreement(&decisions).expect("five samples");
    let variance = mode_match_variance(&decisions).expect("five samples");
    println!(
        "n={SAMPLES} match_baseline={matched} agree_rate={rate} variance={variance} loss={}",
        1.0 - (matched as f64 / SAMPLES as f64)
    );
}

/// 发一次请求并只打印用量。
///
/// **作用**：触发前的两次完整请求共用。
/// **参数**：`client` 是已有密钥的客户端。`request` 是完整请求。`phase` 写入输出前缀。`index` 从 1 起。`cancel` 传给这一次调用。
/// **边界**：没有完成时返回 `false`，不打印响应正文。
async fn print_usage(
    client: &mut DeepSeekClient,
    request: &Value,
    phase: &str,
    index: usize,
    cancel: &CancellationToken,
) -> bool {
    match client.round(request, cancel).await {
        Ok(Round::Completed(completion)) => {
            println!(
                "phase={phase} sample={index} finish={} prompt_tokens={} hit={} miss={}",
                finish_label(&completion.finish_reason),
                prompt_label(completion.usage.prompt_tokens),
                prompt_label(completion.usage.prompt_cache_hit_tokens),
                prompt_label(completion.usage.prompt_cache_miss_tokens)
            );
            true
        }
        other => {
            println!("phase={phase} sample={index} http={}", round_label(&other));
            false
        }
    }
}

/// 没有完成的那一次调用的短类别。
///
/// **作用**：采样失败时只留类别。
/// **参数**：`round` 是 `round` 的结果。传输错误也走这里。
/// **边界**：不打印错误正文。
fn round_label(round: &Result<Round, fluxaxiom_engine_custom::ChatError>) -> String {
    match round {
        Ok(Round::Completed(_)) => "completed".to_owned(),
        Ok(Round::Status { status, .. }) => status.to_string(),
        Ok(Round::Interrupted) => "interrupted".to_owned(),
        Ok(Round::Cancelled) => "cancelled".to_owned(),
        Ok(Round::InvalidToolArguments { .. }) => "invalid_tool_arguments".to_owned(),
        Ok(Round::MalformedToolCall { .. }) => "malformed_tool_call".to_owned(),
        Err(_) => "error".to_owned(),
    }
}

/// 第 5 次请求的未填充完整对完整基线。默认不跑。
///
/// **作用**：同一条已保存请求采样 5 次，打印签名、一致率和方差。
/// **边界**：没有密钥、没有本机 blob 时直接返回。不写 Run。不重测第 1 次请求。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn unfilled_full_baseline_for_mini_inventory_k5() {
    sample_stored_request(REQUEST_K5_SHA256).await;
}

/// 清空第 5 次请求的旧轮思维链后再采样。默认不跑。
///
/// **作用**：先发一次。完成则把它算进 5 次，并和未填充基线比较。
/// **边界**：没有密钥或没有本机 blob 时直接返回。第一次没有完成就停，不再采样。不写 Run。stdout 不含思维链。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn cleared_reasoning_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    let Some((request, cleared)) = blank_assistant_reasoning(request) else {
        println!("skipped reason=missing_messages");
        return;
    };
    println!("assistant_cleared={cleared}");
    if cleared == 0 {
        println!("stopped reason=no_assistant");
        return;
    }
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let cancel = CancellationToken::new();
    let baseline = k5_unfilled_baseline();
    let mut decisions = Vec::new();
    for index in 1..=SAMPLES {
        match client.round(&request, &cancel).await {
            Ok(Round::Completed(completion)) => {
                let decision = decision_of(&completion);
                let matched = decision == baseline;
                println!(
                    "sample={index} finish={} prompt_tokens={} match_baseline={}",
                    finish_label(&completion.finish_reason),
                    prompt_label(completion.usage.prompt_tokens),
                    if matched { "yes" } else { "no" }
                );
                print_decision(index, &decision);
                decisions.push(decision);
            }
            Ok(Round::Status { status, .. }) => {
                println!("sample={index} http={status}");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::Interrupted) => {
                println!("sample={index} http=interrupted");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::Cancelled) => {
                println!("sample={index} http=cancelled");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::InvalidToolArguments { .. }) => {
                println!("sample={index} http=invalid_tool_arguments");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::MalformedToolCall { .. }) => {
                println!("sample={index} http=malformed_tool_call");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Err(_) => {
                println!("sample={index} http=error");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
        }
    }
    let matched = decisions
        .iter()
        .filter(|decision| *decision == &baseline)
        .count();
    let equivalence = matched as f64 / SAMPLES as f64;
    let rate = pairwise_agreement(&decisions).expect("five samples");
    let variance = mode_match_variance(&decisions).expect("five samples");
    println!(
        "n={SAMPLES} agree_rate={rate} variance={variance} equivalence={equivalence} loss={}",
        1.0 - equivalence
    );
}

/// 剩余空间说明只加在第一条 user 末尾，判断不泄露原文。
///
/// **作用**：锁住 T9 的改动位置和提到与否的规则。
/// **边界**：不拨号。夹具文本只留在断言里。
#[test]
fn remaining_notice_appends_only_the_first_user() {
    let request = serde_json::json!({
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {"role": "user", "content": "later"}
        ]
    });
    let noticed = append_text_to_first_user(request, REMAINING_NOTICE).expect("user message");
    assert_eq!(
        noticed["messages"][1]["content"],
        format!("u{REMAINING_NOTICE}")
    );
    assert_eq!(noticed["messages"][2]["content"], "later");
    assert!(mentions_remaining_space(&ChatCompletion {
        reasoning: ObservedText::Absent,
        content: ObservedText::Text("剩余空间不够".to_owned()),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: fluxaxiom_engine_custom::ModelUsage {
            prompt_tokens: TokenCount::Unknown,
            completion_tokens: TokenCount::Unknown,
            prompt_cache_hit_tokens: TokenCount::Unknown,
            prompt_cache_miss_tokens: TokenCount::Unknown,
        },
    }));
    assert!(observed_mentions(&ObservedText::Text(
        "The remaining context is small".to_owned()
    )));
    assert!(!observed_mentions(&ObservedText::Text(
        "read the file".to_owned()
    )));
    assert!(!observed_mentions(&ObservedText::Absent));
}

/// 第 5 次请求声明剩余空间远小于窗口后再采样。默认不跑。
///
/// **作用**：不重测未填充基线。5 次采样和它比较，并记录是否提到剩余空间。
/// **边界**：没有密钥、没有本机 blob 或找不到 user 时直接返回。第一次没有完成就停。不写 Run。stdout 不含正文和思维链。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn remaining_space_notice_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    let Some(request) = append_text_to_first_user(request, REMAINING_NOTICE) else {
        println!("skipped reason=missing_user");
        return;
    };
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let cancel = CancellationToken::new();
    let baseline = k5_unfilled_baseline();
    let mut decisions = Vec::new();
    let mut mentioned = 0usize;
    for index in 1..=SAMPLES {
        match client.round(&request, &cancel).await {
            Ok(Round::Completed(completion)) => {
                let decision = decision_of(&completion);
                let matched = decision == baseline;
                let says = mentions_remaining_space(&completion);
                if says {
                    mentioned += 1;
                }
                println!(
                    "sample={index} finish={} prompt_tokens={} match_baseline={} mentioned={}",
                    finish_label(&completion.finish_reason),
                    prompt_label(completion.usage.prompt_tokens),
                    if matched { "yes" } else { "no" },
                    if says { "yes" } else { "no" }
                );
                print_decision(index, &decision);
                decisions.push(decision);
            }
            Ok(Round::Status { status, .. }) => {
                println!("sample={index} http={status}");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::Interrupted) => {
                println!("sample={index} http=interrupted");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::Cancelled) => {
                println!("sample={index} http=cancelled");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::InvalidToolArguments { .. }) => {
                println!("sample={index} http=invalid_tool_arguments");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Ok(Round::MalformedToolCall { .. }) => {
                println!("sample={index} http=malformed_tool_call");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
            Err(_) => {
                println!("sample={index} http=error");
                println!("stopped reason=not_completed sample={index}");
                return;
            }
        }
    }
    let matched = decisions
        .iter()
        .filter(|decision| *decision == &baseline)
        .count();
    let equivalence = matched as f64 / SAMPLES as f64;
    let rate = pairwise_agreement(&decisions).expect("five samples");
    let variance = mode_match_variance(&decisions).expect("five samples");
    println!(
        "n={SAMPLES} agree_rate={rate} variance={variance} equivalence={equivalence} loss={} mentioned={mentioned}",
        1.0 - equivalence
    );
}

/// 汉字填充只加在第一条 user 末尾。
#[test]
fn han_fill_appends_only_the_first_user() {
    let request = serde_json::json!({
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {"role": "user", "content": "later"}
        ]
    });
    let filled = append_han_to_first_user(request, 3).expect("user message");
    assert_eq!(filled["messages"][1]["content"], "u中中中");
    assert_eq!(filled["messages"][2]["content"], "later");
    assert_eq!(filled["messages"].as_array().expect("messages").len(), 3);
}

/// 第 5 次请求加 1.3 万个「中」后的完整对完整基线。默认不跑。
///
/// **作用**：测量约 16K 的无关填充是否仍做出同一步。
/// **边界**：没有密钥、没有本机 blob 或找不到 user 时直接返回。不写 Run，不打印填充正文。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn han_fill_16k_baseline_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    let Some(filled) = append_han_to_first_user(request, FILL_HAN_CHARS) else {
        println!("skipped reason=missing_user");
        return;
    };
    sample_request(&filled).await;
}

/// 相似干扰不含正确下一步的路径。
#[test]
fn decoy_fill_names_neighbors_and_not_the_target() {
    let text = decoy_fill(2);
    assert!(text.contains("src/inventory.rs"));
    assert!(text.contains("src/storage.rs"));
    assert!(text.contains("README.md"));
    assert!(!text.contains("tests/inventory.rs"));
    assert_eq!(text.matches("src/lib.rs").count(), 2);
    assert_eq!(decoy_repeats_for_target(64_000, 2964), 3391);
    assert_eq!(decoy_repeats_for_target(16_000, 2964), 724);
}

/// 读一次请求的服务端 `prompt_tokens`。
///
/// **作用**：用一小段干扰估计重复次数，不计入五次基线。
/// **参数**：`client` 是已有密钥的 DeepSeek 客户端。`request` 是这一次的填充请求。
/// **边界**：失败时打印类别并返回 `None`。不打印正文或响应摘要。
async fn probe_prompt_tokens(client: &mut DeepSeekClient, request: &Value) -> Option<u64> {
    let cancel = CancellationToken::new();
    match client.round(request, &cancel).await {
        Ok(Round::Completed(completion)) => match completion.usage.prompt_tokens {
            TokenCount::Known(value) => Some(value),
            TokenCount::Unknown => {
                println!("calibrate prompt_tokens=unknown");
                None
            }
        },
        Ok(Round::Status { status, .. }) => {
            println!("calibrate http={status}");
            None
        }
        Ok(Round::Interrupted) => {
            println!("calibrate http=interrupted");
            None
        }
        Ok(Round::Cancelled) => {
            println!("calibrate http=cancelled");
            None
        }
        Ok(Round::InvalidToolArguments { .. }) => {
            println!("calibrate http=invalid_tool_arguments");
            None
        }
        Ok(Round::MalformedToolCall { .. }) => {
            println!("calibrate http=malformed_tool_call");
            None
        }
        Err(_) => {
            println!("calibrate http=error");
            None
        }
    }
}

/// 第 5 次请求加相似路径干扰后的完整对完整基线。默认不跑。
///
/// **作用**：把服务端 `prompt_tokens` 调到约 16000，再采样 5 次。
/// **边界**：没有密钥、没有本机 blob 或找不到 user 时直接返回。不写 Run，不打印干扰全文。校准那一次不计入一致率。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn similar_fill_16k_baseline_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    const UNFILLED_PROMPT: u64 = 2964;
    const TARGET_PROMPT: u64 = 16_000;
    const PROBE_REPEATS: u64 = 40;
    let Some(probe) =
        append_text_to_first_user(request.clone(), &decoy_fill(PROBE_REPEATS as usize))
    else {
        println!("skipped reason=missing_user");
        return;
    };
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let Some(probe_tokens) = probe_prompt_tokens(&mut client, &probe).await else {
        return;
    };
    let Some(added) = probe_tokens
        .checked_sub(UNFILLED_PROMPT)
        .filter(|added| *added > 0)
    else {
        println!(
            "calibrate prompt_tokens={probe_tokens} repeats={PROBE_REPEATS} status=not_larger"
        );
        return;
    };
    let need = TARGET_PROMPT.saturating_sub(UNFILLED_PROMPT);
    let repeats = ((need.saturating_mul(PROBE_REPEATS) + added / 2) / added).max(1);
    println!(
        "calibrate prompt_tokens={probe_tokens} probe_repeats={PROBE_REPEATS} chosen_repeats={repeats}"
    );
    let Some(filled) = append_text_to_first_user(request, &decoy_fill(repeats as usize)) else {
        println!("skipped reason=missing_user");
        return;
    };
    sample_request(&filled).await;
}

/// 按 16K 相似干扰的实测斜率换算重复次数。
///
/// **作用**：40 次重复把 2964 token 加到 3684，即每次重复 18 token。
/// **参数**：`target` 是希望达到的服务端 `prompt_tokens`。`unfilled` 是未填充请求的 `prompt_tokens`，大于 `target` 时返回 1。
/// **边界**：不发请求。正式采样仍记录实际 `prompt_tokens`。
fn decoy_repeats_for_target(target: u64, unfilled: u64) -> usize {
    const TOKENS_PER_REPEAT: u64 = 18;
    let need = target.saturating_sub(unfilled);
    ((need + TOKENS_PER_REPEAT / 2) / TOKENS_PER_REPEAT).max(1) as usize
}

/// 第 5 次请求在 64K、256K、512K 上的两种填充基线。默认不跑。
///
/// **作用**：把 T0 尚未测量的填充长度一次采完。16K 已有记录，这里不重测。
/// **边界**：没有密钥或没有本机 blob 时直接返回。某一档失败时打印类别并继续下一档。不写 Run，不打印填充正文。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn longer_fill_baselines_for_mini_inventory_k5() {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipped reason=missing_key");
        return;
    }
    let Some(request) = stored_request(REQUEST_K5_SHA256) else {
        println!("skipped reason=missing_request");
        return;
    };
    const UNFILLED_PROMPT: u64 = 2964;
    const TARGETS: [(&str, u64); 3] = [("64k", 64_000), ("256k", 256_000), ("512k", 512_000)];
    for (name, target) in TARGETS {
        let chars = target.saturating_sub(UNFILLED_PROMPT) as usize;
        println!("case=han-{name} han_chars={chars}");
        match append_han_to_first_user(request.clone(), chars) {
            Some(filled) => sample_request(&filled).await,
            None => println!("case=han-{name} skipped reason=missing_user"),
        }
        let repeats = decoy_repeats_for_target(target, UNFILLED_PROMPT);
        println!("case=similar-{name} repeats={repeats}");
        match append_text_to_first_user(request.clone(), &decoy_fill(repeats)) {
            Some(filled) => sample_request(&filled).await,
            None => println!("case=similar-{name} skipped reason=missing_user"),
        }
    }
}
