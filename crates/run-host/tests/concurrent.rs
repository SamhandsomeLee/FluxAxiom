//! 两个操作系统进程同时创建 Run，不能因 `SQLITE_BUSY` 失败。
//!
//! 同一进程里两把 `Mutex<Connection>` 串行化的是我们自己的锁，测不到 SQLite 写锁。
//! 所以把本测试二进制再拉起两次，各开一条连接到同一数据目录。
//! WAL + `busy_timeout` 5 秒让后来者等待，而不是立刻报忙。

use std::process::Command;

use fluxaxiom_contract::{EventBody, RunId, RunStatus};
use fluxaxiom_run_host::{HostError, RunHost};

mod common;

/// 子进程看到这个变量就只写库，不再继续拉孙子进程。
const WORKER_ENV: &str = "FLUXAXIOM_HOST_TEST_WORKER";
const DB_ENV: &str = "FLUXAXIOM_HOST_TEST_DB";
/// 每个进程插入的条数。要足以撞上写锁，又不必把测试拖成秒级等待。
const PER_WORKER: usize = 20;

#[test]
fn two_processes_write_without_sqlite_busy() {
    if let Ok(worker) = std::env::var(WORKER_ENV) {
        write_runs(&worker);
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    // 父进程先迁移 schema，并保持连接，模拟另一个 `flux status` 在读的时候有人在写。
    let host = RunHost::open(path).unwrap();

    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for worker in ["a", "b"] {
        children.push(
            Command::new(&executable)
                // `--exact` 避免过滤器把别的测试也跑进子进程。
                .args(["--exact", "two_processes_write_without_sqlite_busy"])
                .env(WORKER_ENV, worker)
                .env(DB_ENV, path)
                .spawn()
                .expect("spawn concurrent writer"),
        );
    }

    for child in &mut children {
        let status = child.wait().expect("wait for concurrent writer");
        assert!(status.success(), "concurrent writer failed: {status}");
    }

    // 每个 worker 使用互不重复的 `request_id`，测的是并发插入，不是幂等碰撞。
    for worker in ["a", "b"] {
        for index in 0..PER_WORKER {
            let request_id = format!("{worker}-{index}");
            host.get_by_request_id(&request_id)
                .unwrap_or_else(|_| panic!("missing run {request_id}"));
        }
    }
}

const DRIVE_WORKER_ENV: &str = "FLUXAXIOM_HOST_TEST_DRIVE";
const RUN_ENV: &str = "FLUXAXIOM_HOST_TEST_RUN";

/// 两个进程同时启动同一个 Accepted Run，只能有一份 `run.started`。
#[test]
fn two_processes_cannot_both_start_one_run() {
    if std::env::var(DRIVE_WORKER_ENV).is_ok() {
        drive_existing_run();
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_race", &workspace))
        .unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            Command::new(&executable)
                .args(["--exact", "two_processes_cannot_both_start_one_run"])
                .env(DRIVE_WORKER_ENV, "1")
                .env(DB_ENV, path)
                .env(RUN_ENV, created.run.run_id.as_str())
                .spawn()
                .expect("spawn drive worker"),
        );
    }
    for child in &mut children {
        let status = child.wait().expect("wait for drive worker");
        assert!(status.success(), "drive worker failed: {status}");
    }
    let events = host.events_after(&created.run.run_id, 0).unwrap();
    let started = events
        .iter()
        .filter(|event| matches!(event.body, EventBody::RunStarted(_)))
        .count();
    assert_eq!(started, 1);
    assert_eq!(
        host.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Completed
    );
}

fn drive_existing_run() {
    let path = std::env::var(DB_ENV).expect("worker database path");
    let run_id = RunId::parse(std::env::var(RUN_ENV).expect("run id")).unwrap();
    let host = RunHost::open(path).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    match runtime.block_on(host.drive_run(&run_id, &RaceEngine)) {
        Ok(_) | Err(HostError::Invalid(_)) => {}
        Err(error) => panic!("unexpected drive error: {error}"),
    }
}

struct RaceEngine;

#[async_trait::async_trait]
impl fluxaxiom_engine_api::EngineAdapter for RaceEngine {
    fn descriptor(&self) -> fluxaxiom_engine_api::EngineDescriptor {
        fluxaxiom_engine_api::EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(
        &self,
        _request: fluxaxiom_engine_api::EngineRunRequest,
        host: std::sync::Arc<dyn fluxaxiom_engine_api::HostPort>,
    ) -> fluxaxiom_engine_api::EngineOutcome {
        let stored = host
            .put_blob(bytes::Bytes::from_static(b"race"), "text/plain")
            .expect("blob");
        host.observe(fluxaxiom_engine_api::EngineObservation::ToolCall {
            tool_name: "read_file".to_owned(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref: stored,
            byte_count: 4,
            truncated: false,
            duration_ms: 1,
        })
        .expect("observe");
        fluxaxiom_engine_api::EngineOutcome::Completed {
            report: fluxaxiom_engine_api::ArtifactDraft {
                artifact_type: "report".to_owned(),
                summary: "done".to_owned(),
            },
        }
    }
}

fn write_runs(worker: &str) {
    let path = std::env::var(DB_ENV).expect("worker database path");
    let path = std::path::Path::new(&path);
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    for index in 0..PER_WORKER {
        host.create_run(common::create_run(format!("{worker}-{index}"), &workspace))
            .unwrap();
    }
}
