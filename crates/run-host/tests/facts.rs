//! 同事务事件、序号补读和内容寻址 blob。
//!
//! blob 在 SQLite 事务外。相同字节必须得到同一引用，并且能读回原文。

use std::sync::Arc;

use async_trait::async_trait;
use fluxaxiom_contract::{EventBody, RunStatus, TerminalReason};
use fluxaxiom_engine_api::{
    ArtifactDraft, BlobRef, EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome,
    EngineRunRequest, FailureReason, HostPort, HostPortError, ModelUsage, TokenCount,
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
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_facts", &workspace))
        .unwrap();
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
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_invalid", &workspace))
        .unwrap();
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
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_fields", &workspace))
        .unwrap();
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
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_empty_type", &workspace))
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
            tool_call_id: "call_test".to_owned(),
            tool_name: "read_file".to_owned(),
            normalized_path_or_query: "src/lib.rs".to_owned(),
            result_ref: result,
            byte_count: 4,
            truncated: false,
            duration_ms: 1,
        })
        .expect("observe");
        EngineOutcome::Completed {
            report: fluxaxiom_engine_api::ArtifactDraft::report("done".to_owned()),
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
            tool_call_id: "call_test".to_owned(),
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
                tool_call_id: String::new(),
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: stored.clone(),
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ToolCall {
                tool_call_id: "call_test".to_owned(),
                tool_name: String::new(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: stored.clone(),
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ToolCall {
                tool_call_id: "call_test".to_owned(),
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: String::new(),
                result_ref: stored.clone(),
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
            EngineObservation::ToolCall {
                tool_call_id: "call_test".to_owned(),
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: BlobRef::new(String::new()),
                byte_count: 0,
                truncated: false,
                duration_ms: 0,
            },
            EngineObservation::ToolCall {
                tool_call_id: "call_test".to_owned(),
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
                context_ref: None,
                context_strategy: None,
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

/// 成对的 context 字段落进事件；半对、空引用和未落盘引用不写事件。终态是 `context_exhausted`。
#[tokio::test]
async fn context_fields_persist_as_a_pair_and_exhaustion_is_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let workspace = common::authorized_workspace(path);
    let host = RunHost::open(path).unwrap();
    let created = host
        .create_run(common::create_run("req_context", &workspace))
        .unwrap();
    host.drive_run(&created.run.run_id, &ContextEngine)
        .await
        .unwrap();

    let events = host.events_after(&created.run.run_id, 0).unwrap();
    let models: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ModelCallCompleted(payload) => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(models.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.body, EventBody::ContextExhausted(_)))
            .count(),
        1
    );
    assert_eq!(models[0].context_ref, None);
    assert_eq!(models[0].context_strategy, None);
    let absent = serde_json::to_value(models[0]).unwrap();
    assert_eq!(absent["context_ref"], serde_json::Value::Null);
    assert_eq!(absent["context_strategy"], serde_json::Value::Null);
    assert_eq!(models[1].context_strategy.as_deref(), Some("identity@1"));
    let context_ref = models[1].context_ref.as_deref().expect("context ref");
    assert_eq!(
        host.read_blob(&BlobRef::new(context_ref)).unwrap(),
        b"SENTINEL_CONTEXT_BODY"
    );
    let encoded = serde_json::to_string(&events).unwrap();
    assert!(!encoded.contains("SENTINEL_CONTEXT_BODY"));
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::RunFailed(payload)
            if payload.terminal_reason == TerminalReason::ContextExhausted
    )));

    let run = host.get_run(&created.run.run_id).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.terminal_reason, Some(TerminalReason::ContextExhausted));
    let artifacts = host.list_artifacts(&created.run.run_id).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].summary, "partial report");
}

struct ContextEngine;

#[async_trait]
impl EngineAdapter for ContextEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: common::FAKE_ENGINE_ID.to_owned(),
            version: common::FAKE_ENGINE_VERSION.to_owned(),
        }
    }

    /// 先写两条合法模型观测，再确认非法 context 组合不会落事件。
    ///
    /// **作用**：证明 Host 把成对字段写入 `model.call_completed`，并把 `ContextExhausted` 收成失败终态。
    /// **参数**：`request` 是这次 drive 的任务，测试不读它。`host` 是正在 Running 的端口，blob 和观测都写在这里。
    /// **边界**：拒绝的观测必须是 `Invalid`。返回带部分报告的 `ContextExhausted`。
    async fn run(&self, _request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let request_ref = host
            .put_blob(bytes::Bytes::from_static(b"request"), "application/json")
            .expect("request blob");
        let response_ref = host
            .put_blob(bytes::Bytes::from_static(b"response"), "application/json")
            .expect("response blob");
        let context_ref = host
            .put_blob(
                bytes::Bytes::from_static(b"SENTINEL_CONTEXT_BODY"),
                "application/json",
            )
            .expect("context blob");
        let usage = ModelUsage {
            prompt_tokens: TokenCount::Unknown,
            completion_tokens: TokenCount::Unknown,
            prompt_cache_hit_tokens: TokenCount::Unknown,
            prompt_cache_miss_tokens: TokenCount::Unknown,
        };
        host.observe(model_call(
            request_ref.clone(),
            response_ref.clone(),
            usage.clone(),
            None,
            None,
        ))
        .expect("absent context");
        host.observe(model_call(
            request_ref.clone(),
            response_ref.clone(),
            usage.clone(),
            Some(context_ref.clone()),
            Some("identity@1".to_owned()),
        ))
        .expect("stored context");
        let missing = BlobRef::new(format!("sha256:{}", "0".repeat(64)));
        for (context_ref, context_strategy) in [
            (Some(context_ref.clone()), None),
            (None, Some("identity@1".to_owned())),
            (
                Some(BlobRef::new(String::new())),
                Some("identity@1".to_owned()),
            ),
            (Some(context_ref), Some(String::new())),
            (Some(missing), Some("identity@1".to_owned())),
        ] {
            assert_eq!(
                host.observe(model_call(
                    request_ref.clone(),
                    response_ref.clone(),
                    usage.clone(),
                    context_ref,
                    context_strategy,
                )),
                Err(HostPortError::Invalid)
            );
        }
        let diagnostic_ref = host
            .put_blob(
                bytes::Bytes::from_static(b"SENTINEL_CONTEXT_BODY"),
                "application/json",
            )
            .unwrap();
        for (context_ref, context_strategy) in [
            (BlobRef::new(""), "identity@1"),
            (BlobRef::new("not-a-blob-ref"), "identity@1"),
            (
                BlobRef::new(format!("sha256:{}", "0".repeat(64))),
                "identity@1",
            ),
            (diagnostic_ref.clone(), ""),
        ] {
            assert_eq!(
                host.observe(EngineObservation::ContextExhausted {
                    context_ref,
                    context_strategy: context_strategy.to_owned(),
                }),
                Err(HostPortError::Invalid)
            );
        }
        host.observe(EngineObservation::ContextExhausted {
            context_ref: diagnostic_ref,
            context_strategy: "identity@1".to_owned(),
        })
        .expect("stored exhaustion diagnostic");
        EngineOutcome::Failed {
            reason: FailureReason::ContextExhausted,
            partial: Some(ArtifactDraft::report("partial report".to_owned())),
        }
    }
}

/// 组装一条模型观测。
///
/// **作用**：context 落库测试少写重复字段。
/// **参数**：`request_ref` 与 `response_ref` 是已准备好的 blob 引用，调用方决定它们是否已落盘。`usage` 原样进入观测。`context_ref` 为 `None` 表示没有记录。`context_strategy` 为 `None` 表示没有策略；空串留给 Host 拒绝。
/// **边界**：不访问 Host。别名和 `finish_reason` 固定为非空短字符串。
fn model_call(
    request_ref: BlobRef,
    response_ref: BlobRef,
    usage: ModelUsage,
    context_ref: Option<BlobRef>,
    context_strategy: Option<String>,
) -> EngineObservation {
    EngineObservation::ModelCall {
        model_alias: "deepseek-flash".to_owned(),
        usage,
        finish_reason: "stop".to_owned(),
        latency_ms: 1,
        request_ref,
        response_ref,
        context_ref,
        context_strategy,
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
                body: "unused".to_owned(),
            },
        }
    }
}
