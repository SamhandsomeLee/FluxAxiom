//! A1.1 存储验收：重开库仍能读到同一条记录；重复 `request_id` 有确定结果。
//!
//! 用临时目录而不是单个临时文件：WAL 会在库旁边生成 `-wal` / `-shm`，
//! 目录在测试结束时一起清掉。

use fluxaxiom_contract::{AttemptStatus, RunStatus, SessionId};
use fluxaxiom_run_host::{HostError, RunHost};

mod common;

/// 关掉连接再打开，证明记录在 SQLite 里，不在进程内存里。
#[test]
fn reopen_reads_same_session_run_and_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let session;
    let created;
    {
        let host = RunHost::open(path).unwrap();
        session = host.create_session().unwrap();
        let workspace = common::authorized_workspace(path);
        let mut request = common::create_run("req_reopen", &workspace);
        request.session_id = Some(session.session_id.clone());
        created = host.create_run(request).unwrap();
        // create_run 只受理，不驱动 Engine，所以停在 Accepted / Starting。
        assert_eq!(created.run.status, RunStatus::Accepted);
        assert_eq!(created.attempt.status, AttemptStatus::Starting);
        // owner 是本进程 PID。A1.1 写入但不拿它判活。
        assert_eq!(created.attempt.owner, std::process::id().to_string());
        assert!(created.run.invariants_hold());
        assert!(created.attempt.invariants_hold());
    }

    let host = RunHost::open(path).unwrap();
    assert_eq!(host.get_session(&session.session_id).unwrap(), session);
    assert_eq!(host.get_run(&created.run.run_id).unwrap(), created.run);
    assert_eq!(
        host.get_attempt(&created.attempt.attempt_id).unwrap(),
        created.attempt
    );
}

/// 幂等键命中已有行就返回那一行，不另插 Run / Attempt。
#[test]
fn duplicate_request_id_returns_same_run() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let host = RunHost::open(path).unwrap();
    let workspace = common::authorized_workspace(path);
    let first = host
        .create_run(common::create_run("req_001", &workspace))
        .unwrap();
    let second = host
        .create_run(common::create_run("req_001", &workspace))
        .unwrap();
    assert_eq!(first, second);
}

/// 契约允许 Run 不挂 Session，但不允许挂一个库里不存在的 Session。
#[test]
fn missing_session_is_not_found() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let host = RunHost::open(path).unwrap();
    let workspace = common::authorized_workspace(path);
    let mut request = common::create_run("req_missing_session", &workspace);
    request.session_id = Some(SessionId::generate());
    let error = host.create_run(request).unwrap_err();
    assert!(matches!(error, HostError::NotFound));
}
