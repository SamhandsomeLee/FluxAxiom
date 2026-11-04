//! T1 离线语料。只读本机那次 Run 的事件和请求 blob。
//!
//! **作用**：数各类消息占比、同文件重读和完全相同的工具结果。
//! **边界**：不拨号，不写 Run，不改档案。没有本机数据时直接返回。stdout 不含 prompt、思维链或工具正文。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fluxaxiom_contract::{ModelCallCompletedPayload, RunId, ToolCallCompletedPayload};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

/// 2026-10-26 的 mini-inventory Run。数据在本机，不在仓库里。
const RUN_ID: &str = "run_01a175b8-f488-77ed-b6ea-153e72ff7616";

/// 一次请求里按角色累加的字节。
///
/// **作用**：最后一次模型请求的构成。
/// **边界**：只计 UTF-8 字节。不保存正文。
struct RoleBytes {
    /// 该角色的消息条数。
    count: usize,
    /// `content` 为字符串时的字节数。不是字符串时记 0，并增加 [`RoleBytes::non_text`]。
    content: usize,
    /// assistant 的 `reasoning_content` 字节数。其他角色为 0。
    reasoning: usize,
    /// `content` 不是字符串的消息条数。
    non_text: usize,
}

/// 工具结果里和 L0、L1 有关的计数。
///
/// **作用**：判断前向去重和作废遮蔽有没有材料。
/// **边界**：不含正文。参数只用于比较，不输出。
struct ToolStats {
    /// `tool.call_completed` 的条数。
    calls: usize,
    /// 与更早一次工具名、规范化参数和结果引用都相同的条数。
    exact_duplicates: usize,
    /// `read_file` 的条数。
    reads: usize,
    /// 同一路径上、第一次之后的 `read_file` 条数。
    reread: usize,
    /// 后一次 `start_line` 不大于同路径先前一次的配对数。没有 `start_line` 的不计入。
    covering_pairs: usize,
    /// 对不上请求里工具调用 id 的条数。
    unmatched: usize,
}

/// 打开本机 Host 库，只读。
///
/// **作用**：T1 读取已提交的事件。
/// **参数**：`directory` 是 Host 数据目录，里面应有 `host.db`。
/// **边界**：用只读标志，不跑迁移，也不扫描租约。打不开时返回 `None`。
fn open_readonly(directory: &Path) -> Option<Connection> {
    Connection::open_with_flags(directory.join("host.db"), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// 按序号读取一个 Run 的事件类型和 payload。
///
/// **作用**：后面只挑模型和工具两类事件。
/// **参数**：`connection` 是已打开的只读库。`run_id` 是要统计的 Run，必须是库里的 `run_id` 文本。
/// **边界**：查询失败返回 `None`。不改库。
fn load_payloads(connection: &Connection, run_id: &str) -> Option<Vec<(String, String)>> {
    let mut statement = connection
        .prepare("SELECT event_type, payload FROM events WHERE run_id = ?1 ORDER BY sequence")
        .ok()?;
    let rows = statement
        .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()?;
    rows.collect::<Result<Vec<_>, _>>().ok()
}

/// 按 `sha256:` 引用读 blob 文件。
///
/// **作用**：取出模型请求的 JSON。
/// **参数**：`directory` 是 Host 数据目录。`reference` 必须以 `sha256:` 开头，后面是 64 位小写十六进制。
/// **边界**：格式不对或文件不在时返回 `None`。不校验哈希。
fn read_blob(directory: &Path, reference: &str) -> Option<Vec<u8>> {
    let hex = reference.strip_prefix("sha256:")?;
    if hex.len() < 4 {
        return None;
    }
    let path = directory
        .join("blobs")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex);
    std::fs::read(path).ok()
}

/// 统计一条请求里各角色的消息。
///
/// **作用**：T1 的消息占比用最后一次请求。
/// **参数**：`request` 是请求 JSON。没有 `messages` 数组时返回 `None`。
/// **边界**：不读取思维链以外的字段原文，也不把任何正文放进返回值。
fn role_bytes(request: &Value) -> Option<BTreeMap<String, RoleBytes>> {
    let messages = request.get("messages")?.as_array()?;
    let mut roles = BTreeMap::<String, RoleBytes>::new();
    for message in messages {
        let role = message.get("role")?.as_str()?.to_owned();
        let entry = roles.entry(role.clone()).or_insert(RoleBytes {
            count: 0,
            content: 0,
            reasoning: 0,
            non_text: 0,
        });
        entry.count += 1;
        match message.get("content").and_then(Value::as_str) {
            Some(text) => entry.content += text.len(),
            None => entry.non_text += 1,
        }
        if role == "assistant" {
            if let Some(text) = message.get("reasoning_content").and_then(Value::as_str) {
                entry.reasoning += text.len();
            }
        }
    }
    Some(roles)
}

/// 把工具参数收成键有序的紧凑文本。
///
/// **作用**：同一组参数不管键序如何都得到同一个比较键。
/// **参数**：`arguments` 是请求里的 JSON 文本。空串和非法 JSON 原样返回。
/// **边界**：不解释路径，不截断合法 JSON。
fn canonical_arguments(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_owned();
    };
    canonical_json(&value)
}

/// 把 JSON 收成键有序的紧凑文本。
///
/// **作用**：[`canonical_arguments`] 的递归部分。
/// **参数**：`value` 是参数或其一层。对象的键按字典序输出。
/// **边界**：不改字符串内容。
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

/// 按消息顺序收集最后一次请求里的工具调用。
///
/// **作用**：这次 Run 的工具事件没有 `tool_call_id`，只能按出现顺序和请求里的 `tool_calls` 对齐。
/// **参数**：`request` 是最后一次模型请求。没有 `messages` 时返回空表。
/// **边界**：跳过没有 `function` 的调用。不保存参数以外的正文。
fn calls_in_order(request: &Value) -> Vec<(String, Option<(String, u64)>)> {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut calls = Vec::new();
    for message in messages {
        let Some(items) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in items {
            let function = call.get("function");
            let name = function
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = function
                .and_then(|item| item.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let canonical = canonical_arguments(arguments);
            let read = if name == "read_file" {
                read_target(arguments)
            } else {
                None
            };
            calls.push((format!("{name} {canonical}"), read));
        }
    }
    calls
}

/// 从各次请求里按调用 id 收集规范化参数和 `read_file` 的路径、起始行。
///
/// **作用**：事件里没有完整参数，覆盖判断要用请求里的 `tool_calls`。
/// **参数**：`requests` 是按时间顺序的请求 JSON。
/// **边界**：缺 id 的调用不收入表。不保存参数以外的正文。
fn argument_index(requests: &[Value]) -> BTreeMap<String, (String, Option<(String, u64)>)> {
    let mut index = BTreeMap::new();
    for request in requests {
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for message in messages {
            let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
                continue;
            };
            for call in calls {
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let function = call.get("function");
                let name = function
                    .and_then(|item| item.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = function
                    .and_then(|item| item.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let canonical = canonical_arguments(arguments);
                let read = if name == "read_file" {
                    read_target(arguments)
                } else {
                    None
                };
                index.insert(id.to_owned(), (format!("{name} {canonical}"), read));
            }
        }
    }
    index
}

/// 读出 `read_file` 的路径和起始行。
///
/// **作用**：同文件重读只比较这两个字段。
/// **参数**：`arguments` 是工具参数 JSON 文本。
/// **边界**：没有 `path` 或 `start_line` 时返回 `None`。不读文件。
fn read_target(arguments: &str) -> Option<(String, u64)> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let path = value.get("path")?.as_str()?.to_owned();
    let start = value.get("start_line")?.as_u64()?;
    Some((path, start))
}

/// 统计完全重复和同文件重读。
///
/// **作用**：L0 看工具名、规范化参数和结果引用是否与更早一次相同。L1 只数 `read_file`。
/// **参数**：`tools` 按事件顺序。`arguments` 的键是 `tool_call_id`，值是比较键和可选的路径与起始行。
/// **边界**：没有调用 id 或对不上表的记入 `unmatched`，不算重复。不输出参数。
fn tool_stats(
    tools: &[ToolCallCompletedPayload],
    arguments: &BTreeMap<String, (String, Option<(String, u64)>)>,
) -> ToolStats {
    let mut seen = BTreeMap::<String, usize>::new();
    let mut reads: Vec<(String, u64)> = Vec::new();
    let mut exact_duplicates = 0usize;
    let mut unmatched = 0usize;
    let mut read_calls = 0usize;
    for tool in tools {
        let Some(id) = tool.tool_call_id.as_deref() else {
            unmatched += 1;
            continue;
        };
        let Some((key, read)) = arguments.get(id) else {
            unmatched += 1;
            continue;
        };
        let identity = format!("{key} {}", tool.result_ref);
        if seen.contains_key(&identity) {
            exact_duplicates += 1;
        }
        *seen.entry(identity).or_insert(0) += 1;
        if tool.tool_name == "read_file" {
            read_calls += 1;
            if let Some((path, start)) = read {
                reads.push((path.clone(), *start));
            }
        }
    }
    let mut seen_reads = Vec::<(String, u64)>::new();
    let mut reread = 0usize;
    let mut covering_pairs = 0usize;
    for (path, start) in &reads {
        let mut prior = false;
        let mut covers = false;
        for (earlier_path, earlier_start) in &seen_reads {
            if earlier_path != path {
                continue;
            }
            prior = true;
            if *start <= *earlier_start {
                covers = true;
            }
        }
        if prior {
            reread += 1;
        }
        if covers {
            covering_pairs += 1;
        }
        seen_reads.push((path.clone(), *start));
    }
    ToolStats {
        calls: tools.len(),
        exact_duplicates,
        reads: read_calls,
        reread,
        covering_pairs,
        unmatched,
    }
}

/// 打印最后一次请求的角色字节。
///
/// **作用**：stdout 只留角色、条数和字节。
/// **参数**：`roles` 来自 [`role_bytes`]。空表也打印总字节 0。
/// **边界**：不打印消息正文。
fn print_roles(roles: &BTreeMap<String, RoleBytes>) {
    let total: usize = roles.values().map(|role| role.content).sum();
    println!("content_bytes={total}");
    for (role, stats) in roles {
        println!(
            "role={role} count={} content_bytes={} reasoning_bytes={} non_text={}",
            stats.count, stats.content, stats.reasoning, stats.non_text
        );
    }
}

/// 夹具上的占比和重复计数。
///
/// **作用**：不依赖本机 Run 也能锁住比较规则。
/// **边界**：不读数据库，不拨号。
#[test]
fn corpus_counts_duplicate_results_and_rereads() {
    let request = serde_json::json!({
        "messages": [
            {"role": "system", "content": "ss"},
            {"role": "user", "content": "u"},
            {
                "role": "assistant",
                "content": "",
                "reasoning_content": "rr",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"start_line\":1,\"path\":\"a.rs\"}"
                    }
                }]
            },
            {"role": "tool", "content": "body", "tool_call_id": "c1"}
        ]
    });
    let roles = role_bytes(&request).expect("messages");
    assert_eq!(roles["system"].content, 2);
    assert_eq!(roles["assistant"].reasoning, 2);
    assert_eq!(roles["tool"].count, 1);
    let arguments = argument_index(&[request]);
    let tools = vec![
        tool("c1", "read_file", "sha256:aaa", 4),
        tool("c2", "read_file", "sha256:aaa", 4),
        tool("c3", "read_file", "sha256:bbb", 4),
    ];
    let mut arguments = arguments;
    arguments.insert(
        "c2".to_owned(),
        (
            format!(
                "read_file {}",
                canonical_arguments(r#"{"path":"a.rs","start_line":1}"#)
            ),
            Some(("a.rs".to_owned(), 1)),
        ),
    );
    arguments.insert(
        "c3".to_owned(),
        (
            format!(
                "read_file {}",
                canonical_arguments(r#"{"path":"a.rs","start_line":10}"#)
            ),
            Some(("a.rs".to_owned(), 10)),
        ),
    );
    let stats = tool_stats(&tools, &arguments);
    assert_eq!(stats.calls, 3);
    assert_eq!(stats.exact_duplicates, 1);
    assert_eq!(stats.reads, 3);
    assert_eq!(stats.reread, 2);
    assert_eq!(stats.covering_pairs, 1);
    assert_eq!(stats.unmatched, 0);
}

/// 造一条工具事件。测试不用真实正文。
///
/// **参数**：`id` 是调用 id。`name` 是工具名。`result` 是结果引用。`bytes` 是交给模型的字节数。
/// **边界**：路径摘要固定为 `p`，不表示真实文件。
fn tool(id: &str, name: &str, result: &str, bytes: u64) -> ToolCallCompletedPayload {
    ToolCallCompletedPayload {
        tool_call_id: Some(id.to_owned()),
        tool_name: name.to_owned(),
        normalized_path_or_query: "p".to_owned(),
        result_ref: result.to_owned(),
        byte_count: bytes,
        truncated: false,
        duration_ms: 0,
    }
}

/// 统计本机 mini-inventory Run。没有数据时跳过。
///
/// **作用**：把 T1 的计数打到 stdout。
/// **边界**：只读 `host.db` 和 blob。不存在或查询失败时打印跳过原因并返回。不打印正文。
#[test]
fn corpus_of_mini_inventory_run() {
    let Some(directory) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        println!("skipped reason=missing_localappdata");
        return;
    };
    let directory = directory.join("FluxAxiom");
    let Some(connection) = open_readonly(&directory) else {
        println!("skipped reason=missing_db");
        return;
    };
    let Some(rows) = load_payloads(&connection, RUN_ID) else {
        println!("skipped reason=query_failed");
        return;
    };
    if rows.is_empty() {
        println!("skipped reason=missing_run");
        return;
    }
    let _ = RunId::parse(RUN_ID).expect("run id shape");
    let mut requests = Vec::new();
    let mut tools = Vec::new();
    for (kind, payload) in rows {
        if kind == "model.call_completed" {
            let Ok(call) = serde_json::from_str::<ModelCallCompletedPayload>(&payload) else {
                println!("skipped reason=bad_model_event");
                return;
            };
            let Some(bytes) = read_blob(&directory, &call.request_ref) else {
                println!("skipped reason=missing_request");
                return;
            };
            let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
                println!("skipped reason=bad_request");
                return;
            };
            requests.push(request);
        } else if kind == "tool.call_completed" {
            let Ok(tool) = serde_json::from_str::<ToolCallCompletedPayload>(&payload) else {
                println!("skipped reason=bad_tool_event");
                return;
            };
            tools.push(tool);
        }
    }
    let Some(last) = requests.last() else {
        println!("skipped reason=no_model_call");
        return;
    };
    let Some(roles) = role_bytes(last) else {
        println!("skipped reason=no_messages");
        return;
    };
    println!("model_calls={}", requests.len());
    print_roles(&roles);
    let stats = if tools.iter().all(|tool| tool.tool_call_id.is_none()) {
        let calls = calls_in_order(last);
        if calls.len() != tools.len() {
            println!(
                "pair_mismatch request_calls={} events={}",
                calls.len(),
                tools.len()
            );
            return;
        }
        for (index, tool) in tools.iter_mut().enumerate() {
            tool.tool_call_id = Some(format!("ord-{index}"));
        }
        let arguments = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| (format!("ord-{index}"), call))
            .collect();
        tool_stats(&tools, &arguments)
    } else {
        tool_stats(&tools, &argument_index(&requests))
    };
    let tool_bytes: u64 = tools.iter().map(|tool| tool.byte_count).sum();
    println!(
        "tool_calls={} tool_bytes={} exact_duplicates={} read_file={} reread={} covering_pairs={} unmatched={}",
        stats.calls,
        tool_bytes,
        stats.exact_duplicates,
        stats.reads,
        stats.reread,
        stats.covering_pairs,
        stats.unmatched
    );
}
