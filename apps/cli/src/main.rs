//! FluxAxiom CLI：组合根。
//!
//! **作用**：组装 `RunHost` 与 `EngineA`，提供 `run` / `status` / `events` / `artifacts`。
//! **边界**：`run` 内监听 Ctrl-C 并调用 `request_cancel`；第二次 Ctrl-C 强退（§4.8 / A3b）。
//! 租约扫描在 `RunHost::open`。默认 Fake Engine；`--engine deepseek` 读 `DEEPSEEK_API_KEY`。

mod data_dir;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use fluxaxiom_contract::{
    Budget, Event, EventBody, ModelUsage, RunId, RunStatus, TokenCount, SCHEMA_VERSION,
};
use fluxaxiom_engine_api::{EngineAdapter, EngineDescriptor};
use fluxaxiom_engine_custom::{
    DeepSeekClient, EngineA, FakeChat, FakeStep, FinishReason, RecordingBackoff, ScriptedTurn,
    SleepBackoff, API_KEY_ENV,
};
use fluxaxiom_run_host::{CreateRun, HostError, RunHost};

/// FluxAxiom 只读仓库分析 CLI（P0a / A3）。
#[derive(Debug, Parser)]
#[command(
    name = "flux",
    version,
    about = "FluxAxiom read-only repository analysis"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 创建 Run、同步驱动 Engine，并打印已提交事件。
    Run {
        /// 授权工作区目录（只读分析目标）。
        #[arg(long)]
        workspace: PathBuf,
        /// 用户任务文本。
        #[arg(long)]
        prompt: String,
        /// 幂等键。省略则每次生成新的。
        #[arg(long)]
        request_id: Option<String>,
        /// Host 数据目录。省略时读 `FLUXAXIOM_DATA_DIR`，再退回平台默认（ADR-021）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// 使用 Fake 或真实 DeepSeek。
        #[arg(long, value_enum, default_value_t = EngineKind::Fake)]
        engine: EngineKind,
        /// 模型调用上限。
        #[arg(long, default_value_t = 8)]
        max_model_calls: u32,
        /// 工具调用上限。
        #[arg(long, default_value_t = 32)]
        max_tool_calls: u32,
    },
    /// 打印 Run 快照。
    Status {
        run_id: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// 打印 durable events；`--follow` 时轮询直到终态。
    Events {
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after_sequence: u64,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// 列出该 Run 的 Artifact 摘要。
    Artifacts {
        run_id: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// CLI 可选的 Engine 实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EngineKind {
    /// 脚本化 Fake，不拨号。默认，便于本地与 CI。
    Fake,
    /// `DeepSeekClient::from_env`，需要 `DEEPSEEK_API_KEY`。
    Deepseek,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("flux: {error}");
        std::process::exit(1);
    }
}

/// 解析参数并分派子命令。
///
/// **作用**：`main` 的唯一业务入口；把 clap 结果交给各 `cmd_*`。
/// **边界**：错误已经格式化成用户可见字符串；不把密钥写进消息。
async fn run() -> Result<(), String> {
    match Cli::parse().command {
        Command::Run {
            workspace,
            prompt,
            request_id,
            data_dir,
            engine,
            max_model_calls,
            max_tool_calls,
        } => {
            cmd_run(
                workspace,
                prompt,
                request_id,
                data_dir,
                engine,
                max_model_calls,
                max_tool_calls,
            )
            .await
        }
        Command::Status { run_id, data_dir } => cmd_status(run_id, data_dir),
        Command::Events {
            run_id,
            after_sequence,
            follow,
            data_dir,
        } => cmd_events(run_id, after_sequence, follow, data_dir).await,
        Command::Artifacts { run_id, data_dir } => cmd_artifacts(run_id, data_dir),
    }
}

/// `flux run`：创建、同步 drive，并边跑边打印事件。
///
/// **参数**：`engine` 为 `Fake` 时不读密钥；`Deepseek` 缺密钥则失败。
/// **边界**：Engine 在碰数据目录之前构造，缺密钥不会留下 Accepted 的孤儿 Run。同一进程内再开一个 `RunHost` 只读轮询事件（WAL）。
/// Ctrl-C 由本命令监听并调用 `request_cancel`（A3b）；第二次 Ctrl-C 直接退出。未以 `Completed` 结束时进程非零退出。
/// 复用已有 `request_id` 且 Run 未终态时继续驱动它：库里的工作区、任务和预算不变，只有 `--engine` 采用本次的。
async fn cmd_run(
    workspace: PathBuf,
    prompt: String,
    request_id: Option<String>,
    data_dir: Option<PathBuf>,
    engine: EngineKind,
    max_model_calls: u32,
    max_tool_calls: u32,
) -> Result<(), String> {
    let (descriptor, engine) = build_engine(engine)?;
    let data_dir = data_dir::resolve(data_dir);
    let host = RunHost::open(&data_dir).map_err(host_err)?;
    let request_id = request_id.unwrap_or_else(new_request_id);
    let existing = host.get_by_request_id(&request_id).is_ok();
    let created = host
        .create_run(CreateRun {
            request_id: request_id.clone(),
            session_id: None,
            input_ref: prompt,
            workspace_ref: workspace.to_string_lossy().into_owned(),
            budget: Budget {
                max_model_calls,
                max_tool_calls,
            },
            engine: descriptor,
        })
        .map_err(host_err)?;
    println!("run_id={}", created.run.run_id.as_str());
    println!("request_id={request_id}");
    println!("data_dir={}", data_dir.display());
    println!("schema_version={SCHEMA_VERSION}");

    let run_id = created.run.run_id.clone();
    if is_terminal(created.run.status) {
        let snapshot = host.snapshot(&run_id).map_err(host_err)?;
        println!(
            "status={:?} terminal_reason={:?} last_sequence={} (idempotent; not re-driven)",
            snapshot.status, snapshot.terminal_reason, snapshot.last_sequence
        );
        return require_completed(snapshot.status, snapshot.terminal_reason);
    }
    if existing {
        eprintln!(
            "flux: request_id {request_id} already exists; continuing the stored run \
             (its workspace, prompt and budget are kept)"
        );
    }

    let poll_dir = data_dir.clone();
    let poll_id = run_id.clone();
    let progress = tokio::spawn(async move {
        let reader = match RunHost::open(&poll_dir) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("flux: progress reader: {error}");
                return;
            }
        };
        if let Err(error) = print_events_until_terminal(&reader, &poll_id, 0).await {
            eprintln!("flux: progress: {error}");
        }
    });

    let cancel_host = host.clone();
    let cancel_id = run_id.clone();
    let interrupt = tokio::spawn(async move {
        // 接线在 CLI：第一次 Ctrl-C → request_cancel；第二次强制退出（§4.8）。
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        eprintln!("flux: interrupt requested; cancelling run");
        // `request_cancel` 会在 SQLite 写锁上阻塞等待，放进阻塞线程，别占住异步 worker。
        match tokio::task::spawn_blocking(move || cancel_host.request_cancel(&cancel_id)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("flux: cancel failed: {error}"),
            Err(error) => eprintln!("flux: cancel failed: {error}"),
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("flux: second interrupt; exiting");
            std::process::exit(130);
        }
    });

    let outcome = match host.drive_run(&run_id, engine.as_ref()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            // Run 没有走到终态时轮询永远不会结束，必须停掉。
            progress.abort();
            interrupt.abort();
            return Err(host_err(error));
        }
    };
    interrupt.abort();
    let _ = progress.await;

    let snapshot = host.snapshot(&run_id).map_err(host_err)?;
    println!(
        "status={:?} terminal_reason={:?} last_sequence={}",
        snapshot.status, snapshot.terminal_reason, snapshot.last_sequence
    );
    println!("outcome={outcome:?}");
    require_completed(snapshot.status, snapshot.terminal_reason)
}

/// 构造本次使用的 Engine 与要写进 Attempt 的身份。
///
/// **作用**：在创建 Run 之前选定实现，避免缺密钥留下 Accepted 孤儿。
/// **参数**：`kind` 来自 `--engine`；`Fake` 不碰网络。
/// **边界**：`Deepseek` 在这里读 `DEEPSEEK_API_KEY`，所以缺密钥发生在任何落库之前。
fn build_engine(kind: EngineKind) -> Result<(EngineDescriptor, Box<dyn EngineAdapter>), String> {
    match kind {
        EngineKind::Fake => Ok((
            EngineDescriptor {
                engine_id: "engine-a-fake".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Box::new(fake_engine()),
        )),
        EngineKind::Deepseek => {
            let client = DeepSeekClient::from_env()
                .map_err(|error| format!("{error} (set {API_KEY_ENV} or use --engine fake)"))?;
            let engine = EngineA::new(client, SleepBackoff);
            Ok((
                EngineDescriptor {
                    engine_id: "engine-a".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                Box::new(engine),
            ))
        }
    }
}

/// 只有 `Completed` 算成功；其余终态让进程以非零码退出。
///
/// **作用**：把 Host 终态收成 CLI 退出码约定。
/// **参数**：`terminal_reason` 仅用于错误消息。
/// **边界**：`Cancelled` / `Failed` 一律非零；不区分取消与失败码。
fn require_completed(
    status: RunStatus,
    terminal_reason: impl std::fmt::Debug,
) -> Result<(), String> {
    if status == RunStatus::Completed {
        Ok(())
    } else {
        Err(format!("run ended {status:?} ({terminal_reason:?})"))
    }
}

/// `flux status`：打印快照关键字段。
///
/// **作用**：只读查询；`open` 仍会扫过期租约。
/// **边界**：不驱动 Engine；找不到 Run 时返回 Host 错误字符串。
fn cmd_status(run_id: String, data_dir: Option<PathBuf>) -> Result<(), String> {
    let host = open_host(data_dir)?;
    let id = parse_run_id(&run_id)?;
    let snapshot = host.snapshot(&id).map_err(host_err)?;
    println!("run_id={}", snapshot.run_id.as_str());
    println!("status={:?}", snapshot.status);
    println!("terminal_reason={:?}", snapshot.terminal_reason);
    println!("last_sequence={}", snapshot.last_sequence);
    println!("workspace_ref={}", snapshot.workspace_ref);
    println!("input_ref={}", snapshot.input_ref);
    println!(
        "budget=model:{} tool:{}",
        snapshot.budget.max_model_calls, snapshot.budget.max_tool_calls
    );
    println!(
        "artifacts={}",
        snapshot
            .artifact_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("updated_at={}", snapshot.updated_at.as_str());
    Ok(())
}

/// `flux events`：打印事件；可选跟随直到终态。
///
/// **作用**：补读或跟随 durable event；`--follow` 复用 `print_events_until_terminal`。
/// **参数**：`after_sequence` 是开区间下界；`0` 表示从头。
/// **边界**：先 `snapshot` 确认 Run 存在，避免空列表与「找错目录」混淆。
async fn cmd_events(
    run_id: String,
    after_sequence: u64,
    follow: bool,
    data_dir: Option<PathBuf>,
) -> Result<(), String> {
    let host = open_host(data_dir)?;
    let id = parse_run_id(&run_id)?;
    // `events_after` 对不存在的 Run 返回空列表，先读快照才分得清「没有事件」和「找错了 Run / 数据目录」。
    host.snapshot(&id).map_err(host_err)?;
    if follow {
        print_events_until_terminal(&host, &id, after_sequence).await
    } else {
        let events = host.events_after(&id, after_sequence).map_err(host_err)?;
        for event in events {
            print_event(&event);
        }
        Ok(())
    }
}

/// `flux artifacts`：打印 Artifact 索引行。
///
/// **作用**：列出类型、`storage_ref` 与短摘要；不打开 blob 正文。
/// **边界**：无产物时打印 `(no artifacts)` 并以零退出。
fn cmd_artifacts(run_id: String, data_dir: Option<PathBuf>) -> Result<(), String> {
    let host = open_host(data_dir)?;
    let id = parse_run_id(&run_id)?;
    let artifacts = host.list_artifacts(&id).map_err(host_err)?;
    if artifacts.is_empty() {
        println!("(no artifacts)");
        return Ok(());
    }
    for artifact in artifacts {
        println!(
            "{} type={} storage_ref={} summary={}",
            artifact.artifact_id.as_str(),
            artifact.artifact_type,
            artifact.storage_ref,
            one_line(&artifact.summary)
        );
    }
    Ok(())
}

/// 轮询打印新事件，直到 Run 终态。
///
/// **作用**：`run` 的进度输出与 `events --follow` 共用。
/// **边界**：按 200ms 轮询 SQLite；终态判定看快照 `status`，不依赖某一条 event。
/// 每轮先读状态、再读事件：终态提交把状态和事件放在同一事务，先看到终态就一定读得到终态之前的全部事件。
async fn print_events_until_terminal(
    host: &RunHost,
    run_id: &RunId,
    mut after: u64,
) -> Result<(), String> {
    loop {
        let terminal = is_terminal(host.snapshot(run_id).map_err(host_err)?.status);
        let events = host.events_after(run_id, after).map_err(host_err)?;
        for event in events {
            after = event.sequence;
            print_event(&event);
        }
        if terminal {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 一行事件摘要。
///
/// **作用**：`run` 进度与 `events` 共用的 stdout 格式。
/// **边界**：只打印序号、类型名与 `subject_id`；不打印 payload 里的引用或正文。
fn print_event(event: &Event) {
    let kind = event_kind(&event.body);
    println!(
        "event seq={} type={} subject={}",
        event.sequence, kind, event.subject_id
    );
}

/// 把契约 `EventBody` 收成稳定类型名字符串。
///
/// **作用**：供 `print_event` 展示；与 schema 的 `event_type` 一致。
/// **边界**：穷尽匹配；新增变体时必须改这里。
fn event_kind(body: &EventBody) -> &'static str {
    match body {
        EventBody::RunAccepted(_) => "run.accepted",
        EventBody::RunStarted(_) => "run.started",
        EventBody::RunCancelling(_) => "run.cancelling",
        EventBody::RunCancelled(_) => "run.cancelled",
        EventBody::RunCompleted(_) => "run.completed",
        EventBody::RunFailed(_) => "run.failed",
        EventBody::AttemptStarted(_) => "attempt.started",
        EventBody::AttemptEnded(_) => "attempt.ended",
        EventBody::AttemptOrphaned(_) => "attempt.orphaned",
        EventBody::ModelCallCompleted(_) => "model.call_completed",
        EventBody::ToolCallCompleted(_) => "tool.call_completed",
        EventBody::ArtifactCreated(_) => "artifact.created",
    }
}

/// Run 是否已离开可驱动状态。
///
/// **作用**：`print_events_until_terminal` 的停止条件。
/// **边界**：`Cancelling` 不算终态，继续轮询。
fn is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
    )
}

/// 解析数据目录并打开 Host。
///
/// **作用**：各子命令共用的入口；内部会 `reap_expired_leases`。
/// **参数**：`data_dir` 为 `--data-dir`；`None` 走环境变量与平台默认。
fn open_host(data_dir: Option<PathBuf>) -> Result<RunHost, String> {
    RunHost::open(data_dir::resolve(data_dir)).map_err(host_err)
}

/// 把 CLI 字符串收成契约 `RunId`。
///
/// **边界**：非法格式返回用户可见错误，不暴露解析细节。
fn parse_run_id(value: &str) -> Result<RunId, String> {
    RunId::parse(value).map_err(|_| format!("invalid run id: {value}"))
}

/// 把 `HostError` 收成 CLI 错误字符串。
///
/// **边界**：依赖 `Display`；不附加堆栈。
fn host_err(error: HostError) -> String {
    error.to_string()
}

/// 把多行摘要压成单行并截断，避免刷屏。
///
/// **参数**：`text` 通常是 Artifact `summary`。
/// **边界**：最多 120 字符；换行换成空格。
fn one_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .take(120)
        .collect()
}

/// 生成本地幂等键。不是契约 ID。
///
/// **作用**：用户省略 `--request-id` 时填 `CreateRun.request_id`。
/// **边界**：含毫秒与 pid，不保证全局唯一，只降低本机碰撞。
fn new_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("req_{millis}_{}", std::process::id())
}

/// 单轮 Fake Engine：直接 `stop`，用于不拨号验收。
///
/// **边界**：不调用真实工具。内容固定，证明 CLI→Host→EngineA 贯通。
fn fake_engine() -> EngineA<FakeChat, RecordingBackoff> {
    EngineA::new(
        FakeChat::script([FakeStep::Turn(ScriptedTurn {
            reasoning_deltas: vec!["plan".to_owned()],
            content_deltas: vec!["Fake Engine A report.".to_owned()],
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: ModelUsage {
                prompt_tokens: TokenCount::Known(1),
                completion_tokens: TokenCount::Known(1),
                prompt_cache_hit_tokens: TokenCount::Unknown,
                prompt_cache_miss_tokens: TokenCount::Unknown,
            },
        })]),
        RecordingBackoff::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::{event_kind, is_terminal, one_line};
    use fluxaxiom_contract::{EmptyPayload, EventBody, RunStatus};

    #[test]
    fn terminal_statuses() {
        assert!(is_terminal(RunStatus::Completed));
        assert!(is_terminal(RunStatus::Failed));
        assert!(!is_terminal(RunStatus::Running));
    }

    #[test]
    fn event_kind_names_match_contract() {
        assert_eq!(
            event_kind(&EventBody::RunAccepted(EmptyPayload {})),
            "run.accepted"
        );
    }

    #[test]
    fn summary_is_single_line() {
        assert_eq!(one_line("a\nb"), "a b");
    }
}
