//! Engine A Loop：Fake Chat + 内存 HostPort。
//!
//! **作用**：覆盖一轮收尾、工具再收尾、额度耗尽、未知工具继续、非法 arguments 先 observe。
//! **边界**：不拨号，不启 `run-host`。预算计数器只在本测试 Host 里。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{ModelUsage, RunId, TokenCount};
use fluxaxiom_engine_api::{
    ArtifactDraft, BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome,
    EngineAdapter, EngineObservation, EngineOutcome, EngineRunRequest, FailureReason, HostPort,
    HostPortError, InputOutcome, InputRequest, ProposedEffect, WorkspaceRoot,
};
use fluxaxiom_engine_custom::{
    CancellationToken, ChatError, ChatTransport, EngineA, FakeChat, FakeStep, FinishReason,
    RecordingBackoff, Round, ScriptedTurn, ToolCall, PROMPT_LAYOUT_VERSION,
};
use tempfile::TempDir;

/// 记录 observe / put_blob / reserve，并按上限扣减。
struct MemoryHost {
    cancel: CancellationToken,
    remaining_model: Mutex<u32>,
    remaining_tool: Mutex<u32>,
    observations: Mutex<Vec<EngineObservation>>,
    blobs: Mutex<Vec<(String, Vec<u8>)>>,
}

impl MemoryHost {
    fn new(model: u32, tool: u32) -> Arc<Self> {
        Arc::new(Self {
            cancel: CancellationToken::new(),
            remaining_model: Mutex::new(model),
            remaining_tool: Mutex::new(tool),
            observations: Mutex::new(Vec::new()),
            blobs: Mutex::new(Vec::new()),
        })
    }

    fn observations(&self) -> Vec<EngineObservation> {
        self.observations.lock().unwrap().clone()
    }
}

#[async_trait]
impl HostPort for MemoryHost {
    /// **作用**：记下观测，供断言模型/工具顺序与用量。
    /// **边界**：不做契约字段校验；空字段仍接受，好测 Loop 自己有没有漏填。
    fn observe(&self, observation: EngineObservation) -> Result<(), HostPortError> {
        self.observations.lock().unwrap().push(observation);
        Ok(())
    }

    /// **作用**：按序号生成 `blob:` 引用并保存字节。
    fn put_blob(&self, bytes: Bytes, _media_type: &str) -> Result<BlobRef, HostPortError> {
        let mut blobs = self.blobs.lock().unwrap();
        let id = format!("blob:{}", blobs.len() + 1);
        blobs.push((id.clone(), bytes.to_vec()));
        Ok(BlobRef::new(id))
    }

    /// **作用**：按 `{model,tool}` 全有或全无扣减。
    /// **边界**：`{0,0}` 允许且不改计数。不够则 `BudgetExhausted`。
    fn reserve(&self, request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        if request.model_calls == 0 && request.tool_calls == 0 {
            return Ok(BudgetLease {
                model_calls: 0,
                tool_calls: 0,
            });
        }
        let mut model = self.remaining_model.lock().unwrap();
        let mut tool = self.remaining_tool.lock().unwrap();
        let next_model = model
            .checked_sub(request.model_calls)
            .ok_or(BudgetExhausted)?;
        let next_tool = tool
            .checked_sub(request.tool_calls)
            .ok_or(BudgetExhausted)?;
        *model = next_model;
        *tool = next_tool;
        Ok(BudgetLease {
            model_calls: request.model_calls,
            tool_calls: request.tool_calls,
        })
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    async fn propose_effect(&self, _effect: ProposedEffect) -> EffectOutcome {
        EffectOutcome::Unsupported
    }

    async fn request_input(&self, _request: InputRequest) -> InputOutcome {
        InputOutcome::Unsupported
    }
}

fn usage() -> ModelUsage {
    ModelUsage {
        prompt_tokens: TokenCount::Known(10),
        completion_tokens: TokenCount::Known(5),
        prompt_cache_hit_tokens: TokenCount::Known(0),
        prompt_cache_miss_tokens: TokenCount::Known(10),
    }
}

fn stop_turn(content: &str) -> FakeStep {
    FakeStep::Turn(ScriptedTurn {
        reasoning_deltas: vec!["think".to_owned()],
        content_deltas: vec![content.to_owned()],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: usage(),
    })
}

fn tool_turn(calls: Vec<ToolCall>, content: &str) -> FakeStep {
    FakeStep::Turn(ScriptedTurn {
        reasoning_deltas: vec![String::new()],
        content_deltas: vec![content.to_owned()],
        tool_calls: calls,
        finish_reason: FinishReason::ToolCalls,
        usage: usage(),
    })
}

fn workspace() -> (TempDir, WorkspaceRoot) {
    let directory = TempDir::new().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello workspace\n").unwrap();
    let root = WorkspaceRoot::open(directory.path()).unwrap();
    (directory, root)
}

async fn run_engine(
    steps: Vec<FakeStep>,
    host: Arc<MemoryHost>,
    workspace: WorkspaceRoot,
) -> EngineOutcome {
    let engine = EngineA::new(FakeChat::script(steps), RecordingBackoff::default());
    engine
        .run(
            EngineRunRequest {
                run_id: RunId::generate(),
                input: "summarize note.txt".to_owned(),
                workspace,
            },
            host,
        )
        .await
}

/// 无工具、`stop`：Completed，报告为 assistant content。
#[tokio::test]
async fn stop_without_tools_completes_with_report() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(vec![stop_turn("final answer")], Arc::clone(&host), root).await;
    assert_eq!(
        outcome,
        EngineOutcome::Completed {
            report: ArtifactDraft::report("final answer".to_owned())
        }
    );
    let observations = host.observations();
    assert_eq!(observations.len(), 1);
    assert!(matches!(
        observations[0],
        EngineObservation::ModelCall {
            finish_reason: ref reason,
            ..
        } if reason == "stop"
    ));
    assert_eq!(PROMPT_LAYOUT_VERSION, "p0a-loop-1");
}

/// 一轮 list_files，再 stop。
///
/// **边界**：两次模型 observe、一次工具 observe；工具 reserve 在模型之后。
#[tokio::test]
async fn tool_round_then_stop() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "list_files".to_owned(),
                    arguments: r#"{"path":"","page":1}"#.to_owned(),
                }],
                "",
            ),
            stop_turn("listed"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Completed {
            report: ArtifactDraft { ref summary, .. }
        } if summary == "listed"
    ));
    let observations = host.observations();
    assert_eq!(observations.len(), 3);
    assert!(matches!(
        observations[0],
        EngineObservation::ModelCall { .. }
    ));
    assert!(matches!(
        observations[1],
        EngineObservation::ToolCall {
            tool_name: ref name,
            ..
        } if name == "list_files"
    ));
    assert!(matches!(
        observations[2],
        EngineObservation::ModelCall { .. }
    ));
}

/// `finish_reason=length` → `Failed(BudgetExhausted)`，非空 content 作 partial。
#[tokio::test]
async fn length_is_budget_exhausted_with_partial() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![FakeStep::Turn(ScriptedTurn {
            reasoning_deltas: vec![],
            content_deltas: vec!["partial text".to_owned()],
            tool_calls: vec![],
            finish_reason: FinishReason::Length,
            usage: usage(),
        })],
        Arc::clone(&host),
        root,
    )
    .await;
    assert_eq!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::BudgetExhausted,
            partial: Some(ArtifactDraft::report("partial text".to_owned())),
        }
    );
}

/// 模型额度在第二轮不够：第一轮已 observe，终态 BudgetExhausted。
#[tokio::test]
async fn model_budget_blocks_second_round() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(1, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "list_files".to_owned(),
                    arguments: r#"{"path":"."}"#.to_owned(),
                }],
                "mid",
            ),
            stop_turn("never"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::BudgetExhausted,
            partial: Some(ArtifactDraft { ref summary, .. })
        } if summary == "mid"
    ));
    assert_eq!(host.observations().len(), 2);
}

/// 工具批额度不够：整批不执行，已有模型 observe。
#[tokio::test]
async fn tool_budget_blocks_the_batch() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 0);
    let outcome = run_engine(
        vec![tool_turn(
            vec![ToolCall {
                id: "call_1".to_owned(),
                name: "list_files".to_owned(),
                arguments: "{}".to_owned(),
            }],
            "before tools",
        )],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::BudgetExhausted,
            partial: Some(_)
        }
    ));
    let observations = host.observations();
    assert_eq!(observations.len(), 1);
    assert!(matches!(
        observations[0],
        EngineObservation::ModelCall { .. }
    ));
}

/// 未知工具名：observe 后继续，下一轮可 stop。
#[tokio::test]
async fn unknown_tool_is_observed_then_loop_continues() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_x".to_owned(),
                    name: "delete_all".to_owned(),
                    arguments: "{}".to_owned(),
                }],
                "",
            ),
            stop_turn("recovered"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Completed {
            report: ArtifactDraft { ref summary, .. }
        } if summary == "recovered"
    ));
    let observations = host.observations();
    assert!(matches!(
        &observations[1],
        EngineObservation::ToolCall {
            tool_name,
            normalized_path_or_query,
            ..
        } if tool_name == "delete_all" && normalized_path_or_query == "delete_all"
    ));
    let blobs = host.blobs.lock().unwrap();
    let tool_blob = blobs
        .iter()
        .find(|(_, body)| std::str::from_utf8(body).unwrap().contains("unknown tool"))
        .expect("unknown tool body");
    assert!(std::str::from_utf8(&tool_blob.1)
        .unwrap()
        .contains("unknown tool: delete_all"));
}

/// 非法 arguments：先 observe（带用量），再 `Failed(ProviderError)`。
#[tokio::test]
async fn invalid_tool_arguments_observe_then_provider_error() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![FakeStep::Turn(ScriptedTurn {
            reasoning_deltas: vec![],
            content_deltas: vec![],
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{".to_owned(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: usage(),
        })],
        Arc::clone(&host),
        root,
    )
    .await;
    assert_eq!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::ProviderError,
            partial: None,
        }
    );
    let observations = host.observations();
    assert_eq!(observations.len(), 1);
    match &observations[0] {
        EngineObservation::ModelCall {
            usage: recorded,
            finish_reason,
            ..
        } => {
            assert_eq!(*recorded, usage());
            assert_eq!(finish_reason, "invalid_tool_arguments");
        }
        other => panic!("expected ModelCall, got {other:?}"),
    }
}

/// 取消令牌在进 Loop 前已触发 → Cancelled。
#[tokio::test]
async fn pre_cancelled_token_returns_cancelled() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    host.cancel.cancel();
    let outcome = run_engine(vec![stop_turn("no")], host, root).await;
    assert_eq!(outcome, EngineOutcome::Cancelled { partial: None });
}

/// `read_file` 能读到工作区文件。
#[tokio::test]
async fn read_file_tool_round_trip() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_r".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"note.txt","start_line":1}"#.to_owned(),
                }],
                "",
            ),
            stop_turn("done"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(outcome, EngineOutcome::Completed { .. }));
    let blobs = host.blobs.lock().unwrap();
    let body = blobs
        .iter()
        .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
        .find(|text| text.contains("hello workspace"))
        .expect("file body in blob");
    assert!(body.contains("tool: read_file"));
}

/// Provider 的非常规结束原因不是成功。
///
/// **边界**：`content_filter` 带着部分正文也要 `Failed(ProviderError)`，正文只作 partial。
#[tokio::test]
async fn unknown_finish_reason_is_provider_error() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![FakeStep::Turn(ScriptedTurn {
            reasoning_deltas: vec![],
            content_deltas: vec!["cut off".to_owned()],
            tool_calls: vec![],
            finish_reason: FinishReason::Other("content_filter".to_owned()),
            usage: usage(),
        })],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::ProviderError,
            partial: Some(ArtifactDraft { ref summary, .. })
        } if summary == "cut off"
    ));
}

/// 最后一轮 content 为空时，不能把上一轮的旁白当报告。
#[tokio::test]
async fn empty_final_content_does_not_reuse_earlier_narration() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "list_files".to_owned(),
                    arguments: "{}".to_owned(),
                }],
                "Let me look",
            ),
            stop_turn(""),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Failed {
            reason: FailureReason::ProviderError,
            partial: Some(ArtifactDraft { ref summary, .. })
        } if summary == "Let me look"
    ));
}

/// `stop` 却带着工具调用：照常执行，不丢调用。
#[tokio::test]
async fn stop_with_tool_calls_still_runs_the_tools() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            FakeStep::Turn(ScriptedTurn {
                reasoning_deltas: vec![],
                content_deltas: vec![String::new()],
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "list_files".to_owned(),
                    arguments: "{}".to_owned(),
                }],
                finish_reason: FinishReason::Stop,
                usage: usage(),
            }),
            stop_turn("after tools"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(
        outcome,
        EngineOutcome::Completed {
            report: ArtifactDraft { ref summary, .. }
        } if summary == "after tools"
    ));
    assert_eq!(host.observations().len(), 3);
}

/// 类型不对的参数不套默认值：正文说明哪个参数非法，Loop 继续。
#[tokio::test]
async fn wrongly_typed_argument_is_reported_not_defaulted() {
    let (_dir, root) = workspace();
    let host = MemoryHost::new(4, 4);
    let outcome = run_engine(
        vec![
            tool_turn(
                vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"note.txt","start_line":"200"}"#.to_owned(),
                }],
                "",
            ),
            stop_turn("noted"),
        ],
        Arc::clone(&host),
        root,
    )
    .await;
    assert!(matches!(outcome, EngineOutcome::Completed { .. }));
    let blobs = host.blobs.lock().unwrap();
    assert!(blobs.iter().any(|(_, body)| {
        String::from_utf8_lossy(body).contains("invalid argument `start_line`")
    }));
    assert!(!blobs
        .iter()
        .any(|(_, body)| String::from_utf8_lossy(body).contains("hello workspace")));
}

/// 先让出一次调度再交给 Fake，让两个 run 在同一实例上真正重叠。
struct YieldingChat(FakeChat);

#[async_trait]
impl ChatTransport for YieldingChat {
    async fn round(
        &mut self,
        request_json: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<Round, ChatError> {
        tokio::task::yield_now().await;
        self.0.round(request_json, cancel).await
    }
}

/// 同一实例上已有 run 在跑时，后到的 run 立刻失败而不是排队。
#[tokio::test]
async fn second_concurrent_run_fails_fast() {
    let (_dir, root) = workspace();
    let (_dir2, root2) = workspace();
    let engine = Arc::new(EngineA::new(
        YieldingChat(FakeChat::script(vec![stop_turn("a"), stop_turn("b")])),
        RecordingBackoff::default(),
    ));
    let request = |workspace| EngineRunRequest {
        run_id: RunId::generate(),
        input: "x".to_owned(),
        workspace,
    };
    let first_host = MemoryHost::new(4, 4);
    let second_host = MemoryHost::new(4, 4);
    let (first, second) = tokio::join!(
        engine.run(request(root), first_host),
        engine.run(request(root2), second_host),
    );
    let failed = [&first, &second]
        .into_iter()
        .filter(|outcome| {
            matches!(
                outcome,
                EngineOutcome::Failed {
                    reason: FailureReason::EngineError,
                    ..
                }
            )
        })
        .count();
    assert_eq!(failed, 1, "{first:?} / {second:?}");
}
