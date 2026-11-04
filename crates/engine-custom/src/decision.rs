//! 一次采样的下一步签名。
//!
//! **作用**：给 `flux ctx replay` 和 T0 共用同一套比较。
//! **边界**：不拨号，不读 blob。不含 prompt、思维链或工具正文。
use crate::chat::{ChatCompletion, FinishReason, ObservedText};
use serde_json::Value;
use std::collections::BTreeMap;

/// 一次采样的下一步签名。
///
/// **作用**：比较工具调用多重集、是否结束，以及参数和正文里的路径。
/// **边界**：不含 prompt、思维链或工具正文。`tools` 按名字和规范化参数排序，重复调用保留。
///
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decision {
    /// 结束原因为 `stop` 时为真。工具调用和截断都不是结束。
    pub ended: bool,
    /// 工具名加规范化参数。顺序不参与比较，重复次数参与。
    pub tools: Vec<(String, String)>,
    /// 参数和正文里抽出的路径。顺序不参与比较，重复路径只留一次。
    pub paths: Vec<String>,
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
pub fn decision_of(completion: &ChatCompletion) -> Decision {
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
pub fn pairwise_agreement(decisions: &[Decision]) -> Option<f64> {
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
pub fn mode_match_variance(decisions: &[Decision]) -> Option<f64> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_object_keys() {
        let value: Value =
            serde_json::from_str(r#"{"start_line":1,"path":"tests/inventory.rs"}"#).unwrap();
        assert_eq!(
            canonical_json(&value),
            r#"{path:"tests/inventory.rs",start_line:1}"#
        );
    }

    #[test]
    fn normalized_arguments_sorts_keys_and_keeps_invalid_text() {
        assert_eq!(
            normalized_arguments(r#"{"start_line":1,"path":"tests/inventory.rs"}"#),
            r#"{path:"tests/inventory.rs",start_line:1}"#
        );
        assert_eq!(normalized_arguments(""), "");
        assert_eq!(normalized_arguments("not-json"), "not-json");
    }

    #[test]
    fn is_path_accepts_only_path_tokens() {
        assert!(is_path("tests/inventory.rs"));
        assert!(is_path("."));
        assert!(is_path(".."));
        assert!(is_path(r"src\lib.rs"));
        assert!(!is_path(""));
        assert!(!is_path("read the file"));
        assert!(!is_path("inventory"));
    }

    #[test]
    fn collect_paths_keeps_only_path_strings() {
        let value: Value = serde_json::from_str(
            r#"{"path":"tests/inventory.rs","name":"inventory","nested":["src/lib.rs",1]}"#,
        )
        .unwrap();
        let mut paths = Vec::new();
        collect_paths(&value, &mut paths);
        paths.sort();
        assert_eq!(paths, ["src/lib.rs", "tests/inventory.rs"]);
    }

    #[test]
    fn paths_in_content_splits_on_punctuation() {
        assert_eq!(
            paths_in_content("see tests/inventory.rs, and src/lib.rs"),
            ["tests/inventory.rs", "src/lib.rs"]
        );
        assert!(paths_in_content("").is_empty());
        assert!(paths_in_content("no path here").is_empty());
    }

    #[test]
    fn decision_of_sorts_tools_and_ignores_reasoning() {
        use crate::chat::{ChatCompletion, FinishReason, ObservedText, ToolCall};
        use fluxaxiom_contract::{ModelUsage, TokenCount};

        let completion = ChatCompletion {
            reasoning: ObservedText::Text("secret src/hidden.rs".to_owned()),
            content: ObservedText::Text("see tests/inventory.rs".to_owned()),
            tool_calls: vec![ToolCall {
                id: "c".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"start_line":1,"path":"tests/inventory.rs"}"#.to_owned(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: ModelUsage {
                prompt_tokens: TokenCount::Unknown,
                completion_tokens: TokenCount::Unknown,
                prompt_cache_hit_tokens: TokenCount::Unknown,
                prompt_cache_miss_tokens: TokenCount::Unknown,
            },
        };
        let decision = decision_of(&completion);
        assert!(!decision.ended);
        assert_eq!(
            decision.tools,
            vec![(
                "read_file".to_owned(),
                r#"{path:"tests/inventory.rs",start_line:1}"#.to_owned()
            )]
        );
        assert_eq!(decision.paths, vec!["tests/inventory.rs".to_owned()]);
    }

    #[test]
    fn pairwise_agreement_uses_pairs_as_the_denominator() {
        let same = Decision {
            ended: false,
            tools: Vec::new(),
            paths: vec!["tests/inventory.rs".to_owned()],
        };
        let other = Decision {
            ended: true,
            tools: Vec::new(),
            paths: Vec::new(),
        };
        assert_eq!(pairwise_agreement(&[]), None);
        assert_eq!(pairwise_agreement(&[same.clone()]), None);
        assert_eq!(pairwise_agreement(&[same.clone(), same.clone()]), Some(1.0));
        let rate = pairwise_agreement(&[same.clone(), same, other]).unwrap();
        assert!((rate - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn normalized_arguments_ignores_whitespace() {
        assert_eq!(
            normalized_arguments(r#"{"path":".","page":1}"#),
            normalized_arguments(r#"{ "page" : 1, "path" : "." }"#)
        );
    }

    #[test]
    fn mode_match_variance_is_zero_when_all_agree() {
        let same = Decision {
            ended: false,
            tools: Vec::new(),
            paths: vec!["tests/inventory.rs".to_owned()],
        };
        let other = Decision {
            ended: true,
            tools: Vec::new(),
            paths: Vec::new(),
        };
        assert_eq!(mode_match_variance(&[same.clone()]), None);
        assert_eq!(
            mode_match_variance(&[same.clone(), same.clone()]),
            Some(0.0)
        );
        let samples = [same.clone(), same.clone(), same.clone(), same, other];
        let variance = mode_match_variance(&samples).unwrap();
        assert!((variance - 0.2).abs() < 1e-12);
    }
}
