//! T0 未填充基线。完整请求对完整请求，各采样 5 次。
//!
//! **作用**：测量 `mini-inventory` 第 1 次和第 5 次模型请求的自一致率。
//! **边界**：不接 Engine，不写 Run、事件或档案。默认不拨号。stdout 不含 prompt、思维链、工具正文或密钥。

use std::collections::BTreeMap;
use std::path::PathBuf;

use fluxaxiom_engine_custom::{
    CancellationToken, ChatCompletion, ChatTransport, DeepSeekClient, FinishReason, ObservedText,
    Round, API_KEY_ENV,
};
use serde_json::Value;

/// 第 1 次请求的 blob。来自 2026-10-26 的 Run。
const REQUEST_K1_SHA256: &str = "f48efa04c0446c5ecae4b5d9de2397f5c3cbb93430c184d975f69e88dd11ad97";

/// 第 5 次请求的 blob。来自同一次 Run。当时的下一步是一次 `read_file`。
const REQUEST_K5_SHA256: &str = "005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc";

/// 同一条完整请求的采样次数。
const SAMPLES: usize = 5;

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
    let mut client = DeepSeekClient::from_env().expect("key checked above");
    let cancel = CancellationToken::new();
    let mut decisions = Vec::new();
    for index in 1..=SAMPLES {
        match client.round(&request, &cancel).await {
            Ok(Round::Completed(completion)) => {
                println!(
                    "sample={index} finish={}",
                    finish_label(&completion.finish_reason)
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

/// 第 5 次请求的未填充完整对完整基线。默认不跑。
///
/// **作用**：同一条已保存请求采样 5 次，打印签名、一致率和方差。
/// **边界**：没有密钥、没有本机 blob 时直接返回。不写 Run。不重测第 1 次请求。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY and the local request blob are present"]
async fn unfilled_full_baseline_for_mini_inventory_k5() {
    sample_stored_request(REQUEST_K5_SHA256).await;
}
