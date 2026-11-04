//! `flux ctx replay`：对已保存的第 k 次请求再采样。
//!
//! **作用**：读请求 blob，经 `ChatTransport::round` 原样发送，打印下一步一致率。
//! **边界**：不调用 `ContextManager::prepare`，不写 Run 或事件。stdout 不含 prompt、思维链或工具正文。

use fluxaxiom_context::IDENTITY_STRATEGY;
use fluxaxiom_contract::{ModelUsage, RunId, TokenCount};
use fluxaxiom_engine_api::BlobRef;
use fluxaxiom_engine_custom::{
    decision_of, mode_match_variance, pairwise_agreement, CancellationToken, ChatTransport,
    Decision, DeepSeekClient, FakeChat, FakeStep, FinishReason, Round, ScriptedTurn, ToolCall,
    API_KEY_ENV,
};
use fluxaxiom_run_host::RunHost;
use serde_json::Value;

use crate::{host_err, open_host, parse_run_id, EngineKind};

/// 对一次 Run 的第 k 次模型请求采样并打印一致率。
///
/// **作用**：`flux ctx replay` 的入口。
/// **参数**：`run_id` 是已存在的 Run。`call` 从 1 起，0 拒绝。`samples` 至少为 1，默认由 clap 设为 5。`strategy` 只能是 `identity@1`。`engine` 为 Fake 时不拨号。`data_dir` 为 `None` 时走环境变量和平台默认。
/// **边界**：先读完请求再采样，不在 await 期间持有 Host。某一次没有完成就停，不打印比率。不写库。
pub async fn replay(
    run_id: String,
    call: u32,
    samples: usize,
    strategy: String,
    engine: EngineKind,
    data_dir: Option<std::path::PathBuf>,
) -> Result<(), String> {
    if strategy != IDENTITY_STRATEGY {
        return Err(format!("only {IDENTITY_STRATEGY} is available"));
    }
    if call == 0 {
        return Err("call starts at 1".to_owned());
    }
    if samples == 0 {
        return Err("samples must be at least 1".to_owned());
    }
    let request = load_request(&run_id, call, data_dir)?;
    let cancel = CancellationToken::new();
    let watcher = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            watcher.cancel();
        }
    });
    let mut transport = transport_for(engine, samples)?;
    let mut decisions = Vec::with_capacity(samples);
    for index in 1..=samples {
        if cancel.is_cancelled() {
            return Err(format!("stopped reason=cancelled sample={index}"));
        }
        match transport.round(&request, &cancel).await {
            Ok(Round::Completed(completion)) => {
                let decision = decision_of(&completion);
                println!(
                    "sample={index} finish={} prompt_tokens={}",
                    finish_label(&completion.finish_reason),
                    prompt_label(completion.usage.prompt_tokens)
                );
                print_decision(index, &decision);
                decisions.push(decision);
            }
            Ok(Round::Status { status, .. }) => {
                return stop(index, &format!("rejected:{status}"));
            }
            Ok(Round::Interrupted) => return stop(index, "interrupted"),
            Ok(Round::Cancelled) => return stop(index, "cancelled"),
            Ok(Round::InvalidToolArguments { .. }) => {
                return stop(index, "invalid_tool_arguments");
            }
            Ok(Round::MalformedToolCall { .. }) => return stop(index, "malformed_tool_call"),
            Err(_) => return stop(index, "error"),
        }
    }
    print_summary(&decisions);
    Ok(())
}

/// 读出第 k 次模型请求的 JSON。
///
/// **作用**：采样开始前取得要原样发送的对象。
/// **参数**：`run_id` 是 Run 文本。`call` 从 1 起。`data_dir` 传给 Host。
/// **边界**：轮次不存在或 blob 不是 JSON 时返回错误。函数返回后不再持有 Host。
fn load_request(
    run_id: &str,
    call: u32,
    data_dir: Option<std::path::PathBuf>,
) -> Result<Value, String> {
    let host = open_host(data_dir)?;
    let id = parse_run_id(run_id)?;
    let reference = request_ref(&host, &id, call)?;
    let bytes = host
        .read_blob(&BlobRef::new(reference))
        .map_err(|_| format!("request blob unavailable for call {call}"))?;
    serde_json::from_slice(&bytes).map_err(|_| format!("request blob is not json for call {call}"))
}

/// 找出第 k 次模型调用的请求引用。
///
/// **作用**：跳过没有模型调用的第 0 轮。
/// **参数**：`host` 只读 trace。`id` 是已解析的 Run。`call` 从 1 起。
/// **边界**：没有这一轮时返回错误，不猜测最后一轮。
fn request_ref(host: &RunHost, id: &RunId, call: u32) -> Result<String, String> {
    let trace = host.trace(id).map_err(host_err)?;
    let round = trace
        .rounds
        .iter()
        .filter(|round| round.model.is_some())
        .nth(call as usize - 1)
        .ok_or_else(|| format!("model call {call} not found"))?;
    Ok(round.model.as_ref().expect("filtered").request_ref.clone())
}

/// 按引擎种类构造传输层。
///
/// **作用**：Fake 回放固定的同一次工具调用；DeepSeek 从环境变量读密钥。
/// **参数**：`engine` 是 CLI 的 `--engine`。`samples` 是 Fake 脚本的步数，必须与采样次数相同。
/// **边界**：缺密钥时不构造客户端。Fake 不访问网络。
fn transport_for(engine: EngineKind, samples: usize) -> Result<Box<dyn ChatTransport>, String> {
    match engine {
        EngineKind::Fake => Ok(Box::new(replay_fake(samples))),
        EngineKind::Deepseek => {
            let client = DeepSeekClient::from_env()
                .map_err(|error| format!("{error} (set {API_KEY_ENV} or use --engine fake)"))?;
            Ok(Box::new(client))
        }
    }
}

/// 造一份每次都返回同一次 `list_files` 的 Fake。
///
/// **作用**：默认引擎下一致率应为 1。
/// **参数**：`samples` 是脚本步数。0 得到空脚本，第一次采样会停住。
/// **边界**：不读真实请求。参数是空路径的 `list_files`。
fn replay_fake(samples: usize) -> FakeChat {
    let usage = ModelUsage {
        prompt_tokens: TokenCount::Known(10),
        completion_tokens: TokenCount::Known(5),
        prompt_cache_hit_tokens: TokenCount::Known(0),
        prompt_cache_miss_tokens: TokenCount::Known(10),
    };
    let step = FakeStep::Turn(ScriptedTurn {
        reasoning_deltas: vec!["look".to_owned()],
        content_deltas: vec![String::new()],
        tool_calls: vec![ToolCall {
            id: "call_list".to_owned(),
            name: "list_files".to_owned(),
            arguments: r#"{"path":""}"#.to_owned(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage,
    });
    FakeChat::script(std::iter::repeat(step).take(samples))
}

/// 打印一次没有完成的采样并返回错误。
///
/// **作用**：让进程以非零码退出，同时不附带比率。
/// **参数**：`index` 从 1 起。`label` 是状态类别，不含错误正文。
/// **边界**：不打印 `error.message`。
fn stop(index: usize, label: &str) -> Result<(), String> {
    println!("sample={index} http={label}");
    Err(format!("stopped reason=not_completed sample={index}"))
}

/// 打印一致率、方差和不同签名个数。
///
/// **作用**：全部采样成功后才调用。
/// **参数**：`decisions` 至少 1 个。只有 1 个时比率和方差写成 `unavailable`。
/// **边界**：不打印正文。
fn print_summary(decisions: &[Decision]) {
    let mut distinct = decisions.to_vec();
    distinct.sort();
    distinct.dedup();
    match (
        pairwise_agreement(decisions),
        mode_match_variance(decisions),
    ) {
        (Some(rate), Some(variance)) => {
            let pairs = decisions.len() * (decisions.len() - 1) / 2;
            println!(
                "n={} pairs={pairs} agree_rate={rate} variance={variance} distinct={}",
                decisions.len(),
                distinct.len()
            );
        }
        _ => println!(
            "n={} agree_rate=unavailable variance=unavailable distinct={}",
            decisions.len(),
            distinct.len()
        ),
    }
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
