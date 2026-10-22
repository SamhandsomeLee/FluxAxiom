//! 同事务事件、序号补读和内容寻址 blob。
//!
//! blob 在 SQLite 事务外。相同字节必须得到同一引用，并且能读回原文。

use std::sync::Arc;

use async_trait::async_trait;
use fluxaxiom_contract::{EventBody, RunStatus, TerminalReason};
use fluxaxiom_engine_api::{
    BlobRef, EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome, EngineRunRequest,
    FailureReason, HostPort, HostPortError,
};
use fluxaxiom_run_host::RunHost;

mod common;

#[test]
fn identical_bytes_share_one_blob() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let first = host.put_blob(b"same", "text/plain").unwrap();
    let second = host.put_blob(b"same", "text/plain").unwrap();
    assert_eq!(first, second);
    assert_eq!(host.read_blob(&first).unwrap(), b"same");
}

#[tokio::test]
async fn sequence_is_contiguous_and_snapshot_lists_the_artifact() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let created = host.create_run(common::create_run("req_facts")).unwrap();
    let input = host
        .read_blob(&BlobRef::new(created.run.input_ref.clone()))
        .unwrap();
    assert_eq!(input, b"analyze runtime");

    host.drive_run(&created.run.run_id, &RecordingEngine)
        .await
        .unwrap();

    let events = host.events_after(&created.run.run_id, 0).unwrap();
    let sequences: Vec<_> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=events.len() as u64).collect::<Vec<_>>());
    assert!(matches!(events[0].body, EventBody::RunAccepted(_)));
    assert!(events
        .iter()
        .any(|event| matches!(event.body, EventBody::ToolCallCompleted(_))));
    assert!(matches!(
        events.last().map(|event| &event.body),
        Some(EventBody::RunCompleted(_))
    ));

    let after = host.events_after(&created.run.run_id, 1).unwrap();
    assert_eq!(after.len(), events.len() - 1);
    assert_eq!(after[0].sequence, 2);

    let snapshot = host.snapshot(&created.run.run_id).unwrap();
    assert_eq!(snapshot.status, RunStatus::Completed);
    assert_eq!(snapshot.last_sequence, *sequences.last().unwrap());
    assert_eq!(snapshot.artifact_ids.len(), 1);
    assert!(snapshot.invariants_hold());
    assert!(host
        .events_after(&created.run.run_id, snapshot.last_sequence)
        .unwrap()
        .is_empty());
}

/// 缺字段时不写 `tool.call_completed`。Run 仍由 Host 收成失败终态。
#[tokio::test]
async fn invalid_observation_writes_no_tool_event() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let created = host.create_run(common::create_run("req_invalid")).unwrap();
    host.drive_run(&created.run.run_id, &InvalidEngine)
        .await
        .unwrap();

    let events = host.events_after(&created.run.run_id, 0).unwrap();
    assert!(events
        .iter()
        .all(|event| !matches!(event.body, EventBody::ToolCallCompleted(_))));
    assert_eq!(
        host.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Failed
    );
}

#[test]
fn empty_media_type_is_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let error = host.put_blob(b"x", "").unwrap_err();
    assert!(matches!(error, fluxaxiom_run_host::HostError::Invalid(_)));
}

/// 空字段和错误的 byte_count 都不能写成 tool event。Run 仍由 Host 收成失败。
#[tokio::test]
async fn blank_fields_and_mismatched_byte_count_are_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let created = host.create_run(common::create_run("req_fields")).unwrap();
    host.drive_run(&created.run.run_id, &RejectingEngine)
        .await
        .unwrap();
    let events = host.events_after(&created.run.run_id, 0).unwrap();
    assert!(events
        .iter()
        .all(|event| !matches!(event.body, EventBody::ToolCallCompleted(_))));
    assert_eq!(
        host.get_run(&created.run.run_id).unwrap().status,
        RunStatus::Failed
    );
}

/// 空的 artifact_type 不能让 Run 停在 Running。
#[tokio::test]
async fn empty_artifact_type_fails_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let host = RunHost::open(directory.path()).unwrap();
    let created = host
        .create_run(common::create_run("req_empty_type"))
        .unwrap();
    let outcome = host
        .drive_run(&created.run.run_id, &EmptyTypeEngine)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: fluxaxiom_engine_api::FailureReason::EngineError,
            ..
        }
    ));
    let run = host.get_run(&created.run.run_id).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.terminal_reason, Some(TerminalReason::EngineError));
    assert!(host
        .snapshot(&created.run.run_id)
        .unwrap()
        .artifact_ids
        .is_empty());
}

struct RecordingEngine;

#[async_trait]
impl EngineAdapter for RecordingEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(&self, request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        assert_eq!(request.input, "analyze runtime");
        let result = host
            .put_blob(bytes::Bytes::from_static(b"body"), "text/plain")
            .expect("blob");
        host.observe(EngineObservation::ToolCall {
            tool_name: "read_file".to_owned(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref: result,
            byte_count: 4,
            truncated: false,
            duration_ms: 1,
        })
        .expect("observe");
        EngineOutcome::Completed {
            report: fluxaxiom_engine_api::ArtifactDraft {
                artifact_type: "report".to_owned(),
                summary: "done".to_owned(),
            },
        }
    }
}

struct InvalidEngine;

#[async_trait]
impl EngineAdapter for InvalidEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let error = host.observe(EngineObservation::ToolCall {
            tool_name: String::new(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref: BlobRef::new(String::new()),
            byte_count: 0,
            truncated: false,
            duration_ms: 0,
        });
        assert_eq!(error, Err(HostPortError::Invalid));
        EngineOutcome::Failed {
            reason: FailureReason::EngineError,
            partial: None,
        }
    }
}

struct RejectingEngine;

#[async_trait]
impl EngineAdapter for RejectingEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let stored = host
            .put_blob(bytes::Bytes::from_static(b"body"), "text/plain")
            .expect("blob");
        let empty_media = host.put_blob(bytes::Bytes::from_static(b"x"), "");
        assert_eq!(empty_media, Err(HostPortError::Invalid));
        for observation in [
            EngineObservation::ToolCall {
                tool_name: String::new(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: stored.clone(),
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ToolCall {
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: String::new(),
                result_ref: stored.clone(),
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ToolCall {
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: BlobRef::new(String::new()),
                byte_count: 0,
                truncated: false,
                duration_ms: 0,
            },
            EngineObservation::ToolCall {
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: stored.clone(),
                byte_count: 3,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ModelCall {
                model_alias: String::new(),
                usage: fluxaxiom_engine_api::ModelUsage {
                    prompt_tokens: fluxaxiom_engine_api::TokenCount::Unknown,
                    completion_tokens: fluxaxiom_engine_api::TokenCount::Unknown,
                    prompt_cache_hit_tokens: fluxaxiom_engine_api::TokenCount::Unknown,
                    prompt_cache_miss_tokens: fluxaxiom_engine_api::TokenCount::Unknown,
                },
                finish_reason: "stop".to_owned(),
                latency_ms: 1,
                request_ref: stored.clone(),
                response_ref: stored.clone(),
            },
        ] {
            assert_eq!(host.observe(observation), Err(HostPortError::Invalid));
        }
        EngineOutcome::Failed {
            reason: FailureReason::EngineError,
            partial: None,
        }
    }
}

struct EmptyTypeEngine;

#[async_trait]
impl EngineAdapter for EmptyTypeEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    async fn run(&self, _request: EngineRunRequest, _host: Arc<dyn HostPort>) -> EngineOutcome {
        EngineOutcome::Completed {
            report: fluxaxiom_engine_api::ArtifactDraft {
                artifact_type: String::new(),
                summary: "unused".to_owned(),
            },
        }
    }
}
