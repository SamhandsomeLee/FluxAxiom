//! Fake Engine 经 `HostPort` 跑通一次 Run。
//!
//! 它不是正式 crate，也不调模型。A1.1 要证明的是：Host 把端口交给 Engine，
//! 再根据 `EngineOutcome` 自己写 Run / Attempt 终态。事件和 blob 留到 A1.2。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{AttemptOutcome, AttemptStatus, RunStatus, TerminalReason};
use fluxaxiom_engine_api::{
    ArtifactDraft, BlobRef, EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome,
    EngineRunRequest, FailureReason, HostPort, HostPortError,
};
use fluxaxiom_run_host::RunHost;

mod common;

/// 测试双：走 `EngineAdapter`，但不调用 Provider。
///
/// `run` 里会碰 `observe` / `put_blob`。A1.1 必须返回 `Unsupported`——
/// 若这里成功，说明端口假装已经持久，后续事件会指向空事实。
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
        // 字段填齐只为匹配端口形状。A11HostPort 不读它们，仍返回 Unsupported。
        // byte_count 为 0 表示交给模型的内容为空，不是缺字段，也不是截断前长度。
        let observe = host.observe(EngineObservation::ToolCall {
            tool_name: "noop".to_owned(),
            normalized_path_or_query: "noop".to_owned(),
            result_ref: BlobRef::new("sha256:unused"),
            byte_count: 0,
            truncated: false,
            duration_ms: 0,
        });
        let blob = host.put_blob(Bytes::from_static(b"x"), "text/plain");
        if observe != Err(HostPortError::Unsupported) || blob.is_ok() {
            return EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: None,
            };
        }
        // 令牌存在即可。A1.1 没有 Ctrl-C，不会被 cancel。
        // 不调用 `reserve`：本阶段不记账，调用即视为耗尽。
        let _cancel = host.cancellation();
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
    let path = directory.path().join("host.db");
    let host = RunHost::open(&path).unwrap();
    let created = host.create_run(common::create_run("req_drive")).unwrap();

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
    let path = directory.path().join("host.db");
    let run_id;
    let attempt_id;
    {
        let host = RunHost::open(&path).unwrap();
        let created = host
            .create_run(common::create_run("req_reopen_drive"))
            .unwrap();
        run_id = created.run.run_id.clone();
        attempt_id = created.attempt.attempt_id.clone();
        host.drive_run(&run_id, &FakeEngine).await.unwrap();
    }

    let host = RunHost::open(&path).unwrap();
    let run = host.get_run(&run_id).unwrap();
    let attempt = host.get_attempt(&attempt_id).unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(attempt.status, AttemptStatus::Ended);
}

/// 已经结束的 Run 再拿同一 `request_id` 创建，必须仍是那条终态记录，不能再开一条 Attempt。
#[tokio::test]
async fn duplicate_request_id_after_drive_keeps_terminal_run() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host.db");
    let host = RunHost::open(&path).unwrap();
    let created = host
        .create_run(common::create_run("req_idempotent_drive"))
        .unwrap();
    host.drive_run(&created.run.run_id, &FakeEngine)
        .await
        .unwrap();

    let again = host
        .create_run(common::create_run("req_idempotent_drive"))
        .unwrap();
    assert_eq!(again.run.run_id, created.run.run_id);
    assert_eq!(again.run.status, RunStatus::Completed);
    assert_eq!(again.attempt.status, AttemptStatus::Ended);
}
