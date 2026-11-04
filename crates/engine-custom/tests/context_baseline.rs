//! T0 未填充基线，以及 T7 清空旧轮思维链后的采样。
//!
//! **作用**：测量 `mini-inventory` 第 1 次和第 5 次模型请求的自一致率。T7 只把第 5 次请求里 assistant 的 `reasoning_content` 改成空字符串。
//! **边界**：不接 Engine，不写 Run、事件或档案。默认不拨号。stdout 不含 prompt、思维链、工具正文或密钥。

use std::collections::BTreeMap;
use std::path::PathBuf;

use fluxaxiom_engine_custom::{
    CancellationToken, ChatCompletion, ChatTransport, DeepSeekClient, FinishReason, ObservedText,
    Round, TokenCount, API_KEY_ENV,
};
use serde_json::Value;

/// 第 1 次请求的 blob。来自 2026-10-26 的 Run。
const REQUEST_K1_SHA256: &str = "f48efa04c0446c5ecae4b5d9de2397f5c3cbb93430c184d975f69e88dd11ad97";

/// 第 5 次请求的 blob。来自同一次 Run。当时的下一步是一次 `read_file`。
const REQUEST_K5_SHA256: &str = "005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc";

/// 同一条完整请求的采样次数。
const SAMPLES: usize = 5;

/// 16K 档加在第一条 user 末尾的汉字数。未填充的第 5 次请求约 2964 token，汉字比例按 1 计。
const FILL_HAN_CHARS: usize = 13_000;

/// 一次采样的下一步签名。
///
/// **作用**：比较工具调用多重集、是否结束，以及参数和正文里的路径。
/// **边界**：不含 prompt、思维链或工具正文。`tools` 按名字和规范化参数排序，重复调用保留。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Decision {
    /// 结束原因为 `stop` 时为真。工具调用和截断都不是结束。
    ended: bool,
    /// 工具名加规范化参数。顺序不参与比较，重复次数参与。
    tools: Vec<(String, String)>,
    /// 参数和正文里抽出的路径。顺序不参与比较，重复路径只留一次。
    paths: Vec<String>,
}

/// 把 JSON 收成键有序的紧凑文本。
///
/// **作用**：同一组参数不管键的书写顺序，都得到同一个字符串。
/// **参数**：`value` 是工具参数或其中一层。对象的键按字典序输出。
/// **边界**：不改字符串内容，不解释路径。数字和布尔按 JSON 原文输出。
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{key}:{}", canonical_json(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let fields: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", fields.join(","))
        }
        other => other.to_string(),
    }
}

/// 规范化一段工具参数。
///
/// **作用**：签名里的参数不保留空白和键序。
/// **参数**：`arguments` 是模型返回的 JSON 文本。空串和非法 JSON 原样返回。
/// **边界**：不读取文件，不截断合法 JSON。
fn normalized_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => canonical_json(&value),
        Err(_) => arguments.to_owned(),
    }
}

/// 判断一段文字是不是路径。
///
/// **作用**：过滤参数和正文里的普通词。
/// **参数**：`text` 是一个字符串叶子或空白分开的词。空串不是路径。
/// **边界**：不打开文件。含空格的句子不是路径。
fn is_path(text: &str) -> bool {
    if text.is_empty() || text.contains(char::is_whitespace) {
        return false;
    }
    text == "." || text == ".." || text.contains('/') || text.contains('\\')
}

/// 收集 JSON 里的路径字符串。
///
/// **作用**：把参数中的路径放进签名。
/// **参数**：`value` 是已解析的参数。`out` 接收路径，调用方负责排序。
/// **边界**：只收 [`is_path`] 接受的字符串。
fn collect_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for child in map.values() {
                collect_paths(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_paths(child, out);
            }
        }
        Value::String(text) if is_path(text) => out.push(text.clone()),
        _ => {}
    }
}

/// 从结论正文抽出路径。
///
/// **作用**：结束时比较结论引用了哪些路径。
/// **参数**：`content` 是给用户看的正文。空串返回空列表。
/// **边界**：只返回路径词，不返回正文本身。
fn paths_in_content(content: &str) -> Vec<String> {
    content
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '，' | '。' | '"' | '\''))
        .filter(|token| is_path(token))
        .map(str::to_owned)
        .collect()
}

/// 把一次成功响应收成签名。
///
/// **作用**：五次采样用同一规则比较。
/// **参数**：`completion` 是已经拼完的响应。工具参数是调用方接受的 JSON。
/// **边界**：不把思维链放进签名。`stop` 才算结束。
fn decision_of(completion: &ChatCompletion) -> Decision {
    let mut tools = Vec::new();
    let mut paths = Vec::new();
    for call in &completion.tool_calls {
        tools.push((call.name.clone(), normalized_arguments(&call.arguments)));
        if let Ok(value) = serde_json::from_str::<Value>(&call.arguments) {
            collect_paths(&value, &mut paths);
        }
    }
    tools.sort();
    if let ObservedText::Text(content) = &completion.content {
        paths.extend(paths_in_content(content));
    }
    paths.sort();
    paths.dedup();
    Decision {
        ended: matches!(completion.finish_reason, FinishReason::Stop),
        tools,
        paths,
    }
}

/// 计算两两一致的比例。
///
/// **作用**：完整对完整的自一致率。
/// **参数**：`decisions` 是同一条请求的采样。少于 2 个时没有配对，返回 `None`。
/// **边界**：分母是配对数，不是采样数。全部相同为 1。
fn pairwise_agreement(decisions: &[Decision]) -> Option<f64> {
    if decisions.len() < 2 {
        return None;
    }
    let mut pairs = 0usize;
    let mut agreed = 0usize;
    for left in 0..decisions.len() {
        for right in (left + 1)..decisions.len() {
            pairs += 1;
            if decisions[left] == decisions[right] {
                agreed += 1;
            }
        }
    }
    Some(agreed as f64 / pairs as f64)
}

/// 计算“是否等于众数”的样本方差。
///
/// **作用**：给自一致率配一个方差。
/// **参数**：`decisions` 至少两个。众数并列时取排序靠后的那个签名。
/// **边界**：全部相同为 0。用无偏分母 `n - 1`。
fn mode_match_variance(decisions: &[Decision]) -> Option<f64> {
    if decisions.len() < 2 {
        return None;
    }
    let mut counts: BTreeMap<&Decision, usize> = BTreeMap::new();
    for decision in decisions {
        *counts.entry(decision).or_insert(0) += 1;
    }
    let mode = counts
        .iter()
        .max_by_key(|(decision, count)| (*count, *decision))
        .map(|(decision, _)| *decision)
        .expect("at least one decision");
    let mean = decisions
        .iter()
        .filter(|decision| *decision == mode)
        .count() as f64
        / decisions.len() as f64;
    let mut square = 0.0;
    for decision in decisions {
        let indicator = if decision == mode { 1.0 } else { 0.0 };
        let delta = indicator - mean;
        square += delta * delta;
    }
    Some(square / (decisions.len() - 1) as f64)
}

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

/// 规范化参数不保留键序和空白。
#[test]
fn canonical_arguments_sort_object_keys() {
    let left = normalized_arguments(r#"{"path":".","page":1}"#);
    let right = normalized_arguments(r#"{ "page" : 1, "path" : "." }"#);
    assert_eq!(left, right);
    assert_eq!(left, "{page:1,path:\".\"}");
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
        normalized_arguments(r#"{"start_line":1,"path":"tests/inventory.rs"}"#),
        k5_unfilled_baseline().tools[0].1
    );
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
