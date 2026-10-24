//! FluxAxiom CLI：组合根。
//!
//! **作用**：组装 `RunHost` 与 `EngineA`，提供 `run` / `status` / `events` / `artifacts`。
//! **边界**：A3a 不做 Ctrl-C → Host 取消、不做租约扫描（ADR-021 / A3b）。默认 Fake Engine；`--engine deepseek` 读 `DEEPSEEK_API_KEY`。

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

/// FluxAxiom 只读仓库分析 CLI（P0a / A3a）。
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
/// 不做 Host 侧 Ctrl-C 取消。Run 没有以 `Completed` 结束时进程以非零码退出，stdout 的状态行照常打印。
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
            "flux: request_id {request_id} already exists; continuing the stored run              (its workspace, prompt and budget are kept)"
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

    let outcome = match host.drive_run(&run_id, engine.as_ref()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            // Run 没有走到终态时轮询永远不会结束，必须停掉。
            progress.abort();
            return Err(host_err(error));
        }
    };
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

/// 一行事件摘要。不打印 payload 里的引用原文以外的敏感大段。
fn print_event(event: &Event) {
    let kind = event_kind(&event.body);
    println!(
        "event seq={} type={} subject={}",
        event.sequence, kind, event.subject_id
    );
}

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

fn is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
    )
}

fn open_host(data_dir: Option<PathBuf>) -> Result<RunHost, String> {
    RunHost::open(data_dir::resolve(data_dir)).map_err(host_err)
}

fn parse_run_id(value: &str) -> Result<RunId, String> {
    RunId::parse(value).map_err(|_| format!("invalid run id: {value}"))
}

fn host_err(error: HostError) -> String {
    error.to_string()
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .take(120)
        .collect()
}

/// 生成本地幂等键。不是契约 ID。
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
