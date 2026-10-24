//! A3b：取消与租约过期孤儿扫描。
//!
//! **作用**：证明 `request_cancel` 收成 `Cancelled`，过期租约经 `open`/`reap` 收成 `Failed(host_restart)`。
//! **边界**：不发真实 SIGINT；不测第二次 Ctrl-C 强退（属 CLI）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{AttemptStatus, EventBody, RunId, RunStatus, TerminalReason};
use fluxaxiom_engine_api::{
    ArtifactDraft, EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome,
    EngineRunRequest, HostPort,
};
use fluxaxiom_run_host::{HostError, RunHost};

mod common;

/// 轮询直到 Run 进入期望状态，最多等 5 秒。
///
/// **作用**：代替固定 sleep，慢机器上也不会在 `drive_run` 还没到 `Running` 时就断言。
async fn wait_for_status(host: &RunHost, run_id: &RunId, want: RunStatus) {
    for _ in 0..500 {
        if host.get_run(run_id).unwrap().status == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run never reached {want:?}");
}

/// 等到取消令牌触发后交回 `Cancelled`。
struct WaitCancelEngine;

#[async_trait]
impl EngineAdapter for WaitCancelEngine {
    /// **作用**：返回与 `create_run` 一致的 Fake 身份，避免 Attempt 校验失败。
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：阻塞在 Host 取消令牌上，模拟长任务被 Ctrl-C 打断。
    /// **参数**：`host` 的 `cancellation()` 必须与 `request_cancel` / 收尸触发的是同一把。
    /// **边界**：不调用模型或工具；收到 cancel 后交回带 partial 的 `Cancelled`。
    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        host.cancellation().cancelled().await;
        EngineOutcome::Cancelled {
            partial: Some(ArtifactDraft {
                artifact_type: "report".to_owned(),
                summary: "interrupted".to_owned(),
            }),
        }
    }
}

/// 等到取消后，仍提交一条工具观测再交回 `Cancelled`。
struct ObserveAfterCancelEngine;

#[async_trait]
impl EngineAdapter for ObserveAfterCancelEngine {
    /// **作用**：返回与 `create_run` 一致的 Fake 身份。
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：模拟被 Ctrl-C 打断的调用在收尾时提交用量观测。
    /// **边界**：观测必须成功；失败就 panic，让测试红。
    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        host.cancellation().cancelled().await;
        let body = Bytes::from_static(b"late");
        let result_ref = host.put_blob(body.clone(), "text/plain").unwrap();
        host.observe(EngineObservation::ToolCall {
            tool_name: "read_file".to_owned(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref,
            byte_count: body.len() as u64,
            truncated: false,
            duration_ms: 1,
        })
        .expect("observation during Cancelling is accepted");
        EngineOutcome::Cancelled { partial: None }
    }
}

/// 立刻 Completed，用来测「后到的 cancel 忽略」。
struct InstantCompleteEngine;

#[async_trait]
impl EngineAdapter for InstantCompleteEngine {
    /// **作用**：返回与 `create_run` 一致的 Fake 身份。
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：不观察取消令牌，立刻交回 `Completed`。
    /// **边界**：用于证明终态提交后 `request_cancel` 不改库。
    async fn run(&self, _request: EngineRunRequest, _host: Arc<dyn HostPort>) -> EngineOutcome {
        EngineOutcome::Completed {
            report: ArtifactDraft {
                artifact_type: "report".to_owned(),
                summary: "done".to_owned(),
            },
        }
    }
}

/// `request_cancel` → Cancelling → Engine 停 → Cancelled。
///
/// **作用**：覆盖 Ctrl-C 主路径的 Host 侧行为。
/// **边界**：不发真实 SIGINT；令牌由测试任务直接 `request_cancel`。
#[tokio::test]
async fn request_cancel_ends_run_as_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_cancel", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let cancel_host = host.clone();
    let cancel_id = run_id.clone();
    let cancel_task = tokio::spawn(async move {
        wait_for_status(&cancel_host, &cancel_id, RunStatus::Running).await;
        cancel_host.request_cancel(&cancel_id).unwrap();
    });
    let outcome = host.drive_run(&run_id, &WaitCancelEngine).await.unwrap();
    cancel_task.await.unwrap();
    assert!(matches!(outcome, EngineOutcome::Cancelled { .. }));
    let run = host.get_run(&run_id).unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.terminal_reason, Some(TerminalReason::Cancelled));
    let events = host.events_after(&run_id, 0).unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunCancelling(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunCancelled(_))));
}

/// 终态已提交后，再 `request_cancel` 不改状态。
///
/// **作用**：证明后到的 cancel 不覆盖先提交的 Completed。
/// **边界**：仍可能触发本进程令牌；本用例 Engine 已结束，只断言库状态。
#[tokio::test]
async fn cancel_after_completed_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_cancel_late", &workspace))
        .unwrap();
    host.drive_run(&created.run.run_id, &InstantCompleteEngine)
        .await
        .unwrap();
    host.request_cancel(&created.run.run_id).unwrap();
    assert_eq!(
        host.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Completed
    );
}

/// 租约打到过去后，再次 `open` 收成 `Failed(host_restart)` + `Orphaned`。
///
/// **作用**：覆盖 A3b 孤儿扫描入口挂在 `open` 上的取舍。
/// **边界**：用第二个 `RunHost::open` 收尸；原 drive 靠随后的 `request_cancel` 醒来，避免挂住 runtime。
#[tokio::test]
async fn expired_lease_is_reaped_on_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_orphan", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let attempt_id = created.attempt.attempt_id.clone();

    // 把 Run 推到 Running 后卡住：用不会结束的 drive，另开任务过期租约并 reap。
    let blocker = host.clone();
    let block_id = run_id.clone();
    let drive = tokio::spawn(async move { blocker.drive_run(&block_id, &WaitCancelEngine).await });
    wait_for_status(&host, &run_id, RunStatus::Running).await;
    host.expire_lease_for_test(&run_id).unwrap();

    let reaped = RunHost::open(path).unwrap();
    let run = reaped.get_run(&run_id).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.terminal_reason, Some(TerminalReason::HostRestart));
    let attempt = reaped.get_attempt(&attempt_id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Orphaned);
    let events = reaped.events_after(&run_id, 0).unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::AttemptOrphaned(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e.body, EventBody::RunFailed(_))));

    // 原 drive 仍在等 cancel；解开以免卡住 runtime。
    host.request_cancel(&run_id).unwrap();
    let _ = drive.await;
}

/// 未过期的 Running 不会被 reap 误杀。
///
/// **作用**：对照过期用例，证明扫描按 `lease_expires_at` 过滤。
/// **边界**：再次 `open` 后状态仍为 `Running`；收尾靠 `request_cancel`。
#[tokio::test]
async fn fresh_lease_is_not_reaped() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_keep", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let drive_host = host.clone();
    let drive_id = run_id.clone();
    let drive =
        tokio::spawn(async move { drive_host.drive_run(&drive_id, &WaitCancelEngine).await });
    wait_for_status(&host, &run_id, RunStatus::Running).await;
    let reader = RunHost::open(path).unwrap();
    assert_eq!(
        reader.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Running
    );
    host.request_cancel(&created.run.run_id).unwrap();
    let _ = drive.await;
}

/// Ctrl-C 可能落在 `Accepted` → `Running` 之间：无论落在哪一侧，Run 都收成 `Cancelled`，取消不能丢。
///
/// **作用**：`drive_run` 在 `prepare_drive` 之前登记令牌，`Accepted` 上的取消只触发令牌；`Running` 之后走正常的 `Cancelling`。
/// **边界**：反复重试 `request_cancel` 直到它成功，两条路径都算通过；不依赖具体落在哪个窗口。
#[tokio::test]
async fn cancel_racing_with_start_is_not_lost() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_cancel_race", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let drive_host = host.clone();
    let drive_id = run_id.clone();
    let drive =
        tokio::spawn(async move { drive_host.drive_run(&drive_id, &WaitCancelEngine).await });
    let mut accepted = false;
    for _ in 0..500 {
        match host.request_cancel(&run_id) {
            Ok(()) => {
                accepted = true;
                break;
            }
            Err(HostError::Invalid(_)) => tokio::task::yield_now().await,
            Err(other) => panic!("unexpected cancel error: {other}"),
        }
    }
    assert!(accepted, "request_cancel never succeeded");
    let outcome = tokio::time::timeout(Duration::from_secs(5), drive)
        .await
        .expect("drive_run must wake up after cancel")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, EngineOutcome::Cancelled { .. }));
    assert_eq!(host.get_run(&run_id).unwrap().status, RunStatus::Cancelled);
}

/// 取消之后 Engine 收尾时提交的观测仍然落库。
///
/// **作用**：被打断那次调用的用量不能因为 Run 已是 `Cancelling` 而丢失。
#[tokio::test]
async fn observation_during_cancelling_is_recorded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_obs_cancelling", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let drive_host = host.clone();
    let drive_id = run_id.clone();
    let drive = tokio::spawn(async move {
        drive_host
            .drive_run(&drive_id, &ObserveAfterCancelEngine)
            .await
    });
    wait_for_status(&host, &run_id, RunStatus::Running).await;
    host.request_cancel(&run_id).unwrap();
    let outcome = drive.await.unwrap().unwrap();
    assert!(matches!(outcome, EngineOutcome::Cancelled { .. }));
    let events = host.events_after(&run_id, 0).unwrap();
    let cancelling = events
        .iter()
        .position(|e| matches!(e.body, EventBody::RunCancelling(_)))
        .expect("run.cancelling");
    let tool = events
        .iter()
        .position(|e| matches!(e.body, EventBody::ToolCallCompleted(_)))
        .expect("late tool.call_completed is recorded");
    assert!(cancelling < tool);
}

/// 同一进程里同一个 Run 不能同时有两个 drive。
///
/// **作用**：第二个 `drive_run` 立刻失败，且不会覆盖或清掉第一个的取消令牌登记。
#[tokio::test]
async fn second_drive_of_the_same_run_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_double_drive", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let drive_host = host.clone();
    let drive_id = run_id.clone();
    let first =
        tokio::spawn(async move { drive_host.drive_run(&drive_id, &WaitCancelEngine).await });
    wait_for_status(&host, &run_id, RunStatus::Running).await;
    let error = host
        .drive_run(&run_id, &WaitCancelEngine)
        .await
        .unwrap_err();
    assert!(matches!(error, HostError::Invalid(_)));
    // 第一个 drive 的令牌登记还在：cancel 仍能把它叫醒。
    host.request_cancel(&run_id).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("first drive wakes up")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, EngineOutcome::Cancelled { .. }));
}

/// `drive_run` 的 future 被丢弃后，取消登记被清掉。
///
/// **作用**：guard 的 Drop 路径；丢弃发生在 Engine 等待中（相当于任务被 abort）。
/// **边界**：之后同一 Run 的 `Running` 上 `request_cancel` 只写库（`Cancelling`），不会去触发已被丢弃的令牌。
#[tokio::test]
async fn dropped_drive_releases_its_registration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_drop_drive", &workspace))
        .unwrap();
    let run_id = created.run.run_id.clone();
    let drive_host = host.clone();
    let drive_id = run_id.clone();
    let drive =
        tokio::spawn(async move { drive_host.drive_run(&drive_id, &WaitCancelEngine).await });
    wait_for_status(&host, &run_id, RunStatus::Running).await;
    drive.abort();
    let _ = drive.await;
    // 登记已撤销：同一进程可以再次对这个 Run 登记新的 drive（Run 已是 Running，prepare 会拒绝，但不是「已在 drive」）。
    let error = host
        .drive_run(&run_id, &WaitCancelEngine)
        .await
        .unwrap_err();
    assert!(
        matches!(error, HostError::Invalid(reason) if reason == "run is not startable"),
        "{error:?}"
    );
}
