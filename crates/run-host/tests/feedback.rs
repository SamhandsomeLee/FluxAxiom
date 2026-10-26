//! A4b 人工反馈：只追加、不改 last_sequence、非法输入拒绝。

use fluxaxiom_contract::{Budget, FeedbackVerdict, RunStatus};
use fluxaxiom_engine_api::EngineDescriptor;
use fluxaxiom_run_host::{CreateRun, RunHost};
use tempfile::tempdir;

fn open_with_workspace() -> (tempfile::TempDir, RunHost, fluxaxiom_contract::RunId) {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "a\n").unwrap();
    let host = RunHost::open(dir.path().join("data")).unwrap();
    let created = host
        .create_run(CreateRun {
            request_id: "fbk-test".to_owned(),
            session_id: None,
            input_ref: "task".to_owned(),
            workspace_ref: workspace.to_string_lossy().into_owned(),
            budget: Budget {
                max_model_calls: 2,
                max_tool_calls: 4,
            },
            engine: EngineDescriptor {
                engine_id: "engine-a".to_owned(),
                version: "0.1.0".to_owned(),
            },
        })
        .unwrap();
    (dir, host, created.run.run_id)
}

#[test]
fn feedback_appends_in_order_and_survives_reopen() {
    let (dir, host, run_id) = open_with_workspace();
    let before = host.snapshot(&run_id).unwrap().last_sequence;
    let first = host
        .add_feedback(&run_id, FeedbackVerdict::NeedsEdit, Some("tweak".into()))
        .unwrap();
    let second = host
        .add_feedback(&run_id, FeedbackVerdict::Usable, None)
        .unwrap();
    let after = host.snapshot(&run_id).unwrap().last_sequence;
    assert_eq!(before, after, "feedback must not change last_sequence");
    let listed = host.list_feedback(&run_id).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].feedback_id, first.feedback_id);
    assert_eq!(listed[1].feedback_id, second.feedback_id);
    assert_eq!(listed[0].verdict, FeedbackVerdict::NeedsEdit);
    assert_eq!(listed[0].note.as_deref(), Some("tweak"));

    drop(host);
    let host = RunHost::open(dir.path().join("data")).unwrap();
    let again = host.list_feedback(&run_id).unwrap();
    assert_eq!(again.len(), 2);
    assert_eq!(again[0].feedback_id, first.feedback_id);
}

#[test]
fn note_too_long_and_missing_run_are_rejected() {
    let (_dir, host, run_id) = open_with_workspace();
    let long = "x".repeat(2001);
    assert!(host
        .add_feedback(&run_id, FeedbackVerdict::Redo, Some(long))
        .is_err());
    let missing = fluxaxiom_contract::RunId::generate();
    assert!(matches!(
        host.add_feedback(&missing, FeedbackVerdict::Usable, None),
        Err(fluxaxiom_run_host::HostError::NotFound)
    ));
    assert_eq!(host.get_run(&run_id).unwrap().status, RunStatus::Accepted);
}

#[test]
fn failed_run_can_receive_feedback() {
    let (_dir, host, run_id) = open_with_workspace();
    // Accepted 尚未 drive 也可评价（状态不限）。
    host.add_feedback(&run_id, FeedbackVerdict::Redo, Some("redo".into()))
        .unwrap();
    assert_eq!(host.list_feedback(&run_id).unwrap().len(), 1);
}
