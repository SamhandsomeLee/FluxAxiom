//! Fake Engine 经 `HostPort` 跑通一次 Run。
//!
//! 它不是正式 crate，也不调模型。要证明的是：Host 把端口交给 Engine，
//! Engine 先 `put_blob` 再 `observe`，Host 再写 Run / Attempt 终态和对应 event。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{AttemptOutcome, AttemptStatus, RunStatus, TerminalReason};
use fluxaxiom_engine_api::{
    ArtifactDraft, EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome,
    EngineRunRequest, FailureReason, HostPort,
};
use fluxaxiom_run_host::RunHost;

mod common;

/// 测试双：走 `EngineAdapter`，但不调用 Provider。
///
/// `run` 里先保存工具结果，再提交观测。`byte_count` 等于交给模型的字节数。
/// `observe` 返回 `Ok` 时，`tool.call_completed` 已经提交。
struct FakeEngine;

#[async_trait]
impl EngineAdapter for FakeEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(&self, request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let bytes = Bytes::from_static(b"noop-result");
        let Ok(result_ref) = host.put_blob(bytes.clone(), "text/plain") else {
            return EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: None,
            };
        };
        // byte_count 是返回给模型的长度，必须和刚才写入的 blob 等长。
        let observe = host.observe(EngineObservation::ToolCall {
            tool_name: "read_file".to_owned(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref,
            byte_count: bytes.len() as u64,
            truncated: false,
            duration_ms: 1,
        });
        if observe.is_err() {
            return EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: None,
            };
        }
        // 令牌存在即可。A1.1 没有 Ctrl-C，不会被 cancel。
        // 不调用 `reserve`：本阶段不记账，调用即视为耗尽。
        let _cancel = host.cancellation();
        assert!(request.workspace.as_path().is_dir());
        EngineOutcome::Completed {
            report: ArtifactDraft {
                artifact_type: "report".to_owned(),
                // Host 本阶段丢弃草稿；字段只为让 `EngineOutcome` 形状完整。
                summary: format!("fake completed {}", request.run_id),
            },
        }
    }
}

/// 驱动结束后，Host 把 Run 标 Completed、Attempt 标 Ended。终态由 Host 写，Engine 不写库。
#[tokio::test]
async fn fake_engine_completes_run_and_ends_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_drive", &workspace))
        .unwrap();

    let outcome = host
        .drive_run(&created.run.run_id, &FakeEngine)
        .await
        .unwrap();
    assert!(matches!(outcome, EngineOutcome::Completed { .. }));

    let run = host.get_run(&created.run.run_id).unwrap();
    let attempt = host.get_attempt(&created.attempt.attempt_id).unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.terminal_reason, Some(TerminalReason::Completed));
    assert_eq!(attempt.status, AttemptStatus::Ended);
    assert_eq!(attempt.outcome, Some(AttemptOutcome::Completed));
    assert!(run.invariants_hold());
    assert!(attempt.invariants_hold());
}

/// 终态必须落在文件里。关掉 Host 再打开，不能退回 Accepted / Starting。
#[tokio::test]
async fn reopen_after_drive_still_completed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let run_id;
    let attempt_id;
    {
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(common::create_run("req_reopen_drive", &workspace))
            .unwrap();
        run_id = created.run.run_id.clone();
        attempt_id = created.attempt.attempt_id.clone();
        host.drive_run(&run_id, &FakeEngine).await.unwrap();
    }

    let host = RunHost::open(path).unwrap();
    let run = host.get_run(&run_id).unwrap();
    let attempt = host.get_attempt(&attempt_id).unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(attempt.status, AttemptStatus::Ended);
}

/// 已经结束的 Run 再拿同一 `request_id` 创建，必须仍是那条终态记录，不能再开一条 Attempt。
#[tokio::test]
async fn duplicate_request_id_after_drive_keeps_terminal_run() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_idempotent_drive", &workspace))
        .unwrap();
    host.drive_run(&created.run.run_id, &FakeEngine)
        .await
        .unwrap();

    let again = host
        .create_run(common::create_run("req_idempotent_drive", &workspace))
        .unwrap();
    assert_eq!(again.run.run_id, created.run.run_id);
    assert_eq!(again.run.status, RunStatus::Completed);
    assert_eq!(again.attempt.status, AttemptStatus::Ended);
}

/// 工作区目录打不开时不进入 Running，Engine 不会被调用。
#[tokio::test]
async fn missing_workspace_leaves_run_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let host = RunHost::open(path).unwrap();
    let mut request = common::create_run("req_missing_ws", path);
    request.workspace_ref = path.join("does-not-exist").to_string_lossy().into_owned();
    let created = host.create_run(request).unwrap();
    let error = host
        .drive_run(&created.run.run_id, &FakeEngine)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        fluxaxiom_run_host::HostError::Invalid("workspace root is unavailable")
    ));
    assert_eq!(
        host.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Accepted
    );
    let events = host.events_after(&created.run.run_id, 0).unwrap();
    assert!(events
        .iter()
        .all(|event| !matches!(event.body, fluxaxiom_contract::EventBody::RunStarted(_))));
}
