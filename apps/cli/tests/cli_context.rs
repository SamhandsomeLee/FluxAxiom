//! 上下文耗尽的持久诊断：真实 Host、FakeChat 与 CLI，不访问网络。

use std::process::Command;

use assert_cmd::prelude::*;
use fluxaxiom_contract::{
    AttemptId, AttemptOutcome, Budget, EventBody, ModelUsage, RunStatus, TerminalReason, TokenCount,
};
use fluxaxiom_engine_api::{BlobRef, EngineAdapter};
use fluxaxiom_engine_custom::{
    EngineA, FakeChat, FakeStep, FinishReason, RecordingBackoff, ScriptedTurn, ToolCall,
};
use fluxaxiom_run_host::{CreateRun, RunHost};
use serde_json::json;

/// 首次及后续轮耗尽的记录经事件关联到 Run/Attempt，重开后可读，不增加模型计数。
#[tokio::test]
async fn exhaustion_diagnostic_survives_reopen_without_a_phantom_model_call() {
    for after_first_call in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("note.txt"),
            "SENTINEL_TOOL_BODY_DO_NOT_LEAK",
        )
        .unwrap();
        let steps = if after_first_call {
            vec![FakeStep::Turn(ScriptedTurn {
                content_deltas: vec!["partial analysis".to_owned()],
                reasoning_deltas: vec!["SENTINEL_REASONING_DO_NOT_LEAK".to_owned()],
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"note.txt"}"#.to_owned(),
                }],
                finish_reason: FinishReason::ToolCalls,
                usage: ModelUsage {
                    prompt_tokens: TokenCount::Known(710_000),
                    completion_tokens: TokenCount::Known(2_000),
                    prompt_cache_hit_tokens: TokenCount::Unknown,
                    prompt_cache_miss_tokens: TokenCount::Unknown,
                },
            })]
        } else {
            vec![]
        };
        // 脚本没有终止响应；若超限后仍调用模型，Fake 会因脚本耗尽而失败。
        let engine = EngineA::new(FakeChat::script(steps), RecordingBackoff::default());
        let host = RunHost::open(data.path()).unwrap();
        let created = host
            .create_run(CreateRun {
                request_id: "context-exhaustion".to_owned(),
                session_id: None,
                input_ref: if after_first_call {
                    "SENTINEL_PROMPT_DO_NOT_LEAK".to_owned()
                } else {
                    format!("SENTINEL_PROMPT_DO_NOT_LEAK{}", "a".repeat(2_400_000))
                },
                workspace_ref: workspace.path().to_string_lossy().into_owned(),
                budget: Budget {
                    max_model_calls: 4,
                    max_tool_calls: 4,
                },
                engine: engine.descriptor(),
            })
            .unwrap();
        host.drive_run(&created.run.run_id, &engine).await.unwrap();
        drop(host);

        let host = RunHost::open(data.path()).unwrap();
        let run = host.get_run(&created.run.run_id).unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.terminal_reason, Some(TerminalReason::ContextExhausted));
        let events = host.events_after(&run.run_id, 0).unwrap();
        let diagnostics: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.body, EventBody::ContextExhausted(_)))
            .collect();
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = diagnostics[0];
        assert_eq!(diagnostic.run_id, run.run_id);
        let attempt = host
            .get_attempt(&AttemptId::parse(&diagnostic.subject_id).unwrap())
            .unwrap();
        assert_eq!(attempt.run_id, run.run_id);
        assert_eq!(attempt.outcome, Some(AttemptOutcome::Failed));
        assert_eq!(attempt.error_reason.as_deref(), Some("context_exhausted"));
        let EventBody::ContextExhausted(payload) = &diagnostic.body else {
            unreachable!()
        };
        let record_bytes = host.read_blob(&BlobRef::new(&payload.context_ref)).unwrap();
        let record: serde_json::Value = serde_json::from_slice(&record_bytes).unwrap();
        assert_eq!(payload.context_strategy, "l0@1");
        assert_eq!(record["context_strategy"], payload.context_strategy);
        assert_eq!(record["schema_version"], 3);
        assert_eq!(record["spec"]["target"], 64000);
        assert_eq!(record["spec"]["hard_limit"], 711065);
        assert_eq!(record["spec"]["soft_trigger"], 128000);
        assert_eq!(record["spec"]["protect_rounds"], 2);
        let min_free = record["spec"]["min_free"].as_u64().expect("min_free");
        let tokens = record["estimate"]["tokens"].as_u64().expect("tokens");
        assert_eq!(min_free, 32_000u64.max(tokens / 4));
        assert!(record["estimate"]["tokens"].as_u64().unwrap() > 711_065);
        assert_eq!(
            record["estimate"]["source"],
            if after_first_call {
                "usage"
            } else {
                "estimated"
            }
        );
        assert_eq!(
            record["estimate"]["anchor_round"],
            if after_first_call {
                json!(1)
            } else {
                json!(null)
            }
        );
        assert_eq!(
            serde_json::to_value(&diagnostic.body).unwrap(),
            json!({
                "event_type": "context.exhausted",
                "payload": { "context_ref": payload.context_ref, "context_strategy": "l0@1" }
            })
        );
        let ended_sequence = events
            .iter()
            .find(|event| matches!(event.body, EventBody::AttemptEnded(_)))
            .unwrap()
            .sequence;
        assert!(diagnostic.sequence < ended_sequence);
        assert!(matches!(
            events.last().unwrap().body,
            EventBody::RunFailed(_)
        ));
        let trace = host.trace(&run.run_id).unwrap();
        assert_eq!(trace.totals.model_calls, u32::from(after_first_call));
        assert_eq!(trace.totals.tool_calls, u32::from(after_first_call));
        let model_refs: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.body {
                EventBody::ModelCallCompleted(model) => model.context_ref.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(model_refs.len(), usize::from(after_first_call));
        assert!(model_refs
            .iter()
            .all(|reference| *reference != payload.context_ref));
        let artifacts = host.list_artifacts(&run.run_id).unwrap();
        assert_eq!(artifacts.len(), usize::from(after_first_call));
        if after_first_call {
            assert_eq!(
                host.read_blob(&BlobRef::new(&artifacts[0].storage_ref))
                    .unwrap(),
                b"partial analysis"
            );
        }

        let output = Command::cargo_bin("flux")
            .unwrap()
            .current_dir(workspace.path())
            .args(["events", run.run_id.as_str(), "--data-dir"])
            .arg(data.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("context.exhausted"));
        for text in [
            String::from_utf8(record_bytes).unwrap(),
            serde_json::to_string(&events).unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        ] {
            for marker in [
                "SENTINEL_PROMPT_DO_NOT_LEAK",
                "SENTINEL_REASONING_DO_NOT_LEAK",
                "SENTINEL_TOOL_BODY_DO_NOT_LEAK",
            ] {
                assert!(
                    !text.contains(marker),
                    "diagnostic or default output leaked {marker}"
                );
            }
        }
    }
}
