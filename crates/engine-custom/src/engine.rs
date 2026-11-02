//! Engine A：最小只读 Agent Loop。
//!
//! **作用**：实现 `EngineAdapter::run`。按 Host 预算预留模型与工具调用，串起 Chat Completions 与三个只读工具，经 `HostPort` 写 blob 与观测。
//! **边界**：不写 SQLite，不发 `run.*` / `attempt.*`。密钥、`reasoning_content`、prompt 与文件全文只进 blob，不进观测 payload。默认测试用 Fake，不拨号。

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_context::{
    AssistantTurn, ContextManager, Round as ContextRound, ToolCall as ContextToolCall,
    ToolResult as ContextToolResult, Transcript, View,
};
use fluxaxiom_contract::ModelUsage;
use fluxaxiom_engine_api::{
    ArtifactDraft, BudgetRequest, EngineAdapter, EngineDescriptor, EngineObservation,
    EngineOutcome, EngineRunRequest, FailureReason, HostPort, HostPortError, WorkspaceRoot,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::chat::{
    replay_reasoning, ChatCompletion, ChatError, ChatRequest, FinishReason, Message, ObservedText,
    ToolCall, ToolSpec, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
};
use crate::retry::{execute, Backoff, ChatTransport};
use crate::tools::{list_files, read_file, search_code, ToolError, ToolOutput};

/// 布局版本字面量的唯一来源。`concat!` 只吃字面量，所以用宏同时喂给常量和 system 文本。
macro_rules! layout_version {
    () => {
        "p0a-loop-1"
    };
}

/// 写入 `model.call_completed` 与样本记录的布局版本。system 与工具 schema 变更时递增。
pub const PROMPT_LAYOUT_VERSION: &str = layout_version!();

/// Engine 身份。Host 写入 Attempt 的 `engine_id`。
const ENGINE_ID: &str = "engine-a";

/// 与 crate 版本对齐的实现版本字符串。
const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 固定在消息列表最前的 system 文本。与 [`PROMPT_LAYOUT_VERSION`] 一起版本化。
const SYSTEM_PROMPT: &str = concat!(
    "You are FluxAxiom Engine A in read-only analysis mode (prompt_layout_version=",
    layout_version!(),
    ").\n",
    "Use only the provided tools to inspect the authorized workspace.\n",
    "Do not invent file contents. When finished, answer in plain text without further tool calls.\n",
);

/// 最小只读 Agent Loop。
///
/// **作用**：持有可替换的 Chat 传输与退避策略，实现 `EngineAdapter`。
/// **边界**：`transport` / `backoff` 在 `run` 期间独占。同一实例已有 `run` 在跑时，后到的 `run` 立刻 `Failed(EngineError)`，不排队。
pub struct EngineA<T, B> {
    transport: Mutex<T>,
    backoff: Mutex<B>,
}

impl<T, B> EngineA<T, B> {
    /// 注入传输层与退避策略。
    ///
    /// **参数**：`transport` 负责一次已序列化的 Chat 请求；`backoff` 只处理 429/5xx 间隔。
    /// **边界**：不读环境变量。真实 DeepSeek 客户端由调用方构造后传入。
    #[must_use]
    pub fn new(transport: T, backoff: B) -> Self {
        Self {
            transport: Mutex::new(transport),
            backoff: Mutex::new(backoff),
        }
    }
}

#[async_trait]
impl<T, B> EngineAdapter for EngineA<T, B>
where
    T: ChatTransport + Send,
    B: Backoff + Send,
{
    /// **作用**：回报实现身份，供 Host 写入 Attempt。
    /// **边界**：不是权限声明。
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: ENGINE_ID.to_owned(),
            version: ENGINE_VERSION.to_owned(),
        }
    }

    /// **作用**：在已授权工作区上跑只读分析 Loop，经 `host` 预留预算并提交观测。
    /// **参数**：`request.input` 是用户任务；`request.workspace` 已由 Host `open`。
    /// **边界**：模型调用前先 `prepare`。超硬上限先保存记录并 observe context.exhausted，再返回 Failed(ContextExhausted)，不占模型额度。
    /// 保存记录或观测失败时返回 EngineError，不能把诊断丢失当成已成功记录的耗尽。
    /// 通过后再 `reserve({1,0})`，一批 N 个工具前 `reserve({0,N})`；失败不归还。
    /// `finish_reason=length` → `Failed(BudgetExhausted)`；畸形 / 非法工具 JSON → 先 observe 再 `Failed(ProviderError)`；
    /// 未知工具名 observe 后继续。报告是最后一轮 assistant `content`；最后一轮 content 为空或 `finish_reason` 不是
    /// `stop` / `tool_calls` / `length` 时 → `Failed(ProviderError)`，更早轮次的文字只作 partial。只要本轮带工具调用就执行，不看 `stop`。
    /// 取消令牌在每轮模型前、每个工具前检查，工具执行期间触发也会立即返回 `Cancelled`。
    /// `latency_ms` 是 `execute` 的端到端耗时，含 429/5xx 的退避等待与失败重试。
    /// 模型额度按 Loop 轮次预留，`execute` 内部的 429/5xx 重试不再预留。HTTP 失败（400、重试耗尽）、流中断、
    /// 连接失败的调用占用已预留的额度，但不产生 `ModelCall` 观测：没有可信的用量与结束原因可记，Host 侧
    /// 的模型调用数可能小于已预留数，成本报告不含这类调用。是否补写 `usage=Unknown` 的观测留待 EA-02 决定。
    async fn run(&self, request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome {
        let cancel = host.cancellation();
        let Ok(manager) = deepseek_manager() else {
            return EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: None,
            };
        };
        let mut history = Transcript {
            system: SYSTEM_PROMPT.to_owned(),
            user: request.input,
            rounds: Vec::new(),
        };
        let tools = tool_specs();
        let mut last_content = String::new();
        let (Ok(mut transport), Ok(mut backoff)) =
            (self.transport.try_lock(), self.backoff.try_lock())
        else {
            return EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: None,
            };
        };

        loop {
            if cancel.is_cancelled() {
                return EngineOutcome::Cancelled {
                    partial: partial_report(&last_content),
                };
            }

            let prepared = match manager.prepare(&history, DEFAULT_MAX_TOKENS) {
                Ok(prepared) => prepared,
                Err(exhausted) => {
                    return match observe_context_exhausted(&host, &exhausted.record, &last_content)
                    {
                        Ok(()) => EngineOutcome::Failed {
                            reason: FailureReason::ContextExhausted,
                            partial: partial_report(&last_content),
                        },
                        Err(outcome) => outcome,
                    };
                }
            };
            if host
                .reserve(BudgetRequest {
                    model_calls: 1,
                    tool_calls: 0,
                })
                .is_err()
            {
                return EngineOutcome::Failed {
                    reason: FailureReason::BudgetExhausted,
                    partial: partial_report(&last_content),
                };
            }

            let chat_request = ChatRequest::new(DEFAULT_MODEL, messages_from_view(&prepared.view))
                .with_tools(tools.clone());
            let request_bytes = match serde_json::to_vec(&chat_request) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return EngineOutcome::Failed {
                        reason: FailureReason::EngineError,
                        partial: partial_report(&last_content),
                    };
                }
            };

            let started = Instant::now();
            let completion =
                match execute(&mut *transport, &chat_request, &mut *backoff, &cancel).await {
                    Ok(completion) => completion,
                    Err(ChatError::Cancelled) => {
                        return EngineOutcome::Cancelled {
                            partial: partial_report(&last_content),
                        };
                    }
                    Err(ChatError::MalformedToolCall { usage }) => {
                        return fail_after_usage_observe(
                            &host,
                            &request_bytes,
                            usage,
                            "malformed_tool_call",
                            started.elapsed().as_millis() as u64,
                            &last_content,
                            &prepared.record,
                        );
                    }
                    Err(ChatError::InvalidToolArguments { usage }) => {
                        return fail_after_usage_observe(
                            &host,
                            &request_bytes,
                            usage,
                            "invalid_tool_arguments",
                            started.elapsed().as_millis() as u64,
                            &last_content,
                            &prepared.record,
                        );
                    }
                    Err(
                        ChatError::Rejected { .. }
                        | ChatError::Unavailable { .. }
                        | ChatError::StreamInterrupted
                        | ChatError::Transport,
                    ) => {
                        return EngineOutcome::Failed {
                            reason: FailureReason::ProviderError,
                            partial: partial_report(&last_content),
                        };
                    }
                    Err(ChatError::MissingCredential | ChatError::NotEncodable) => {
                        return EngineOutcome::Failed {
                            reason: FailureReason::EngineError,
                            partial: partial_report(&last_content),
                        };
                    }
                };
            let latency_ms = started.elapsed().as_millis() as u64;

            if let Err(outcome) = observe_model(
                &host,
                ModelCapture {
                    request_bytes: &request_bytes,
                    usage: completion.usage.clone(),
                    finish_reason: finish_reason_text(&completion.finish_reason),
                    response: &completion_wire(&completion),
                    latency_ms,
                    record: &prepared.record,
                },
                &last_content,
            ) {
                return outcome;
            }

            let content = observed_as_string(&completion.content);
            if !content.is_empty() {
                last_content = content.clone();
            }

            let replay = replay_reasoning(completion.reasoning.clone(), true);
            history.rounds.push(ContextRound {
                assistant: AssistantTurn {
                    content: content.clone(),
                    reasoning: replay.reasoning_content.unwrap_or_default(),
                    tool_calls: completion
                        .tool_calls
                        .iter()
                        .map(|call| ContextToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect(),
                },
                tools: Vec::new(),
                usage: completion.usage.clone(),
                trailing_user: String::new(),
            });

            match completion.finish_reason {
                FinishReason::Length => {
                    return EngineOutcome::Failed {
                        reason: FailureReason::BudgetExhausted,
                        partial: partial_report(&last_content),
                    };
                }
                _ if !completion.tool_calls.is_empty() => {
                    let count = completion.tool_calls.len() as u32;
                    if host
                        .reserve(BudgetRequest {
                            model_calls: 0,
                            tool_calls: count,
                        })
                        .is_err()
                    {
                        return EngineOutcome::Failed {
                            reason: FailureReason::BudgetExhausted,
                            partial: partial_report(&last_content),
                        };
                    }
                    for call in &completion.tool_calls {
                        if cancel.is_cancelled() {
                            return EngineOutcome::Cancelled {
                                partial: partial_report(&last_content),
                            };
                        }
                        match run_one_tool(&host, &request.workspace, call, &last_content, &cancel)
                            .await
                        {
                            Ok(tool_result) => match history.rounds.last_mut() {
                                Some(round) => round.tools.push(tool_result),
                                None => {
                                    return EngineOutcome::Failed {
                                        reason: FailureReason::EngineError,
                                        partial: partial_report(&last_content),
                                    };
                                }
                            },
                            Err(outcome) => return outcome,
                        }
                    }
                }
                FinishReason::ToolCalls | FinishReason::Stop => {
                    if content.is_empty() {
                        return EngineOutcome::Failed {
                            reason: FailureReason::ProviderError,
                            partial: partial_report(&last_content),
                        };
                    }
                    return EngineOutcome::Completed {
                        report: ArtifactDraft::report(content),
                    };
                }
                FinishReason::Other(_) => {
                    return EngineOutcome::Failed {
                        reason: FailureReason::ProviderError,
                        partial: partial_report(&last_content),
                    };
                }
            }
        }
    }
}

/// 载入嵌入的 DeepSeek Flash 档案。
///
/// **作用**：`run` 在进入循环前调用一次。
/// **边界**：档案文本无法解析时返回错误。调用方收成 `EngineError`，不是 `ContextExhausted`。
fn deepseek_manager() -> Result<ContextManager, fluxaxiom_context::ProfileError> {
    Ok(ContextManager::new(fluxaxiom_context::deepseek_flash()?))
}

/// 保存 prepare 拒绝发送请求时的诊断记录。
///
/// **作用**：把失败记录的 blob 引用经独立观测关联到 Run，不伪造模型调用。
/// **参数**：`host` 提供存储和观测端口；`record` 是失败 prepare 返回的完整记录；
/// `last_content` 是已有的部分报告，空串表示没有报告。
/// **边界**：不预留模型额度、不发送请求；存储或观测失败时返回 EngineError 并保留部分报告。
fn observe_context_exhausted(
    host: &Arc<dyn HostPort>,
    record: &fluxaxiom_context::ContextRecord,
    last_content: &str,
) -> Result<(), EngineOutcome> {
    let context_ref = put_context_record(host, record, last_content)?;
    observe(
        host,
        EngineObservation::ContextExhausted {
            context_ref,
            context_strategy: record.context_strategy.clone(),
        },
        last_content,
    )
}

/// 将 context 记录写成 JSON blob。
///
/// **作用**：已发出的模型调用和未发出的耗尽诊断共用同一记录写入路径。
/// **参数**：`host` 保存记录正文；`record` 只含数值与引用；`last_content` 在失败时用作部分报告。
/// **边界**：序列化或存储失败时返回 EngineError，不静默丢弃记录；本函数不写观测。
fn put_context_record(
    host: &Arc<dyn HostPort>,
    record: &fluxaxiom_context::ContextRecord,
    last_content: &str,
) -> Result<fluxaxiom_engine_api::BlobRef, EngineOutcome> {
    let bytes = serde_json::to_vec(record).map_err(|_| EngineOutcome::Failed {
        reason: FailureReason::EngineError,
        partial: partial_report(last_content),
    })?;
    put_bytes(host, &bytes, "application/json", last_content)
}

/// 把视图收成 Chat Completions 消息。
///
/// **作用**：`prepare` 成功后交给 `ChatRequest`。
/// **参数**：`view` 是这次要发送的视图。system 与首条 user 原样放入。各轮 assistant 使用正文、思维链和工具调用。工具结果只使用正文和调用 ID。
/// **边界**：`trailing_user` 为空串时不另起 user 消息。工具名、参数和 `result_ref` 不进入 tool 消息。空的工具调用列表由 `Message` 省略。
pub(crate) fn messages_from_view(view: &View) -> Vec<Message> {
    let mut messages = vec![
        Message::System {
            content: view.system.clone(),
        },
        Message::User {
            content: view.user.clone(),
        },
    ];
    for round in &view.rounds {
        messages.push(Message::Assistant {
            content: round.assistant.content.clone(),
            reasoning_content: round.assistant.reasoning.clone(),
            tool_calls: round
                .assistant
                .tool_calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        });
        for tool in &round.tools {
            messages.push(Message::Tool {
                content: tool.body.clone(),
                tool_call_id: tool.call_id.clone(),
            });
        }
        if !round.trailing_user.is_empty() {
            messages.push(Message::User {
                content: round.trailing_user.clone(),
            });
        }
    }
    messages
}

/// 一次已经准备好、即将写入观测的模型调用。
///
/// **作用**：把 `observe_model` 的请求、响应和 context 记录收在一起。
/// **边界**：不含 prompt 以外的密钥。`record` 本身不含正文。
struct ModelCapture<'a> {
    /// 已序列化的 Chat 请求。调用方持有这段字节。
    request_bytes: &'a [u8],
    /// 这次响应的用量。未知项保持 `Unknown`。
    usage: ModelUsage,
    /// 写入观测的结束原因。空串由调用方避免。
    finish_reason: String,
    /// 响应 blob 的 JSON。思维链在这里，不进事件 payload。
    response: &'a Value,
    /// 这次调用的墙钟毫秒。0 允许。
    latency_ms: u64,
    /// 这次 `prepare` 的记录。不含正文。
    record: &'a fluxaxiom_context::ContextRecord,
}

/// 把一次模型响应写成 blob 并 observe。成功路径与畸形 / 非法 arguments 路径共用。
///
/// **作用**：请求、响应和 context 记录各自落 blob，观测里只留引用。
/// **参数**：`capture` 是这次要落盘的请求、响应和记录。`last_content` 只在失败时用作部分报告，空串表示没有部分报告。
/// **边界**：失败时返回已构造好的 `EngineOutcome`，调用方直接 return。记录与请求都用 `application/json`。不在这里重试。
fn observe_model(
    host: &Arc<dyn HostPort>,
    capture: ModelCapture<'_>,
    last_content: &str,
) -> Result<(), EngineOutcome> {
    let request_ref = put_bytes(
        host,
        capture.request_bytes,
        "application/json",
        last_content,
    )?;
    let response_bytes = match serde_json::to_vec(capture.response) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(EngineOutcome::Failed {
                reason: FailureReason::EngineError,
                partial: partial_report(last_content),
            });
        }
    };
    let response_ref = put_bytes(host, &response_bytes, "application/json", last_content)?;
    let context_ref = put_context_record(host, capture.record, last_content)?;
    observe(
        host,
        EngineObservation::ModelCall {
            model_alias: DEFAULT_MODEL.to_owned(),
            usage: capture.usage,
            finish_reason: capture.finish_reason,
            latency_ms: capture.latency_ms,
            request_ref,
            response_ref,
            context_ref: Some(context_ref),
            context_strategy: Some(capture.record.context_strategy.clone()),
        },
        last_content,
    )
}

/// 畸形 / 非法 arguments 路径：仍有用量，先 observe 再 `Failed(ProviderError)`。
///
/// **作用**：这次请求已经发出，记录与请求、响应一起提交。
/// **参数**：`finish_reason` 是写入观测的非空标记（不是 Provider 原文）。`latency_ms` 是这次调用的墙钟毫秒，0 允许。`last_content` 只在失败时用作部分报告。`record` 是发出这次请求前的 `prepare` 记录。
/// **边界**：响应 blob 只含结束标记与用量，不含半截工具调用。
fn fail_after_usage_observe(
    host: &Arc<dyn HostPort>,
    request_bytes: &[u8],
    usage: ModelUsage,
    finish_reason: &str,
    latency_ms: u64,
    last_content: &str,
    record: &fluxaxiom_context::ContextRecord,
) -> EngineOutcome {
    let response = json!({
        "finish_reason": finish_reason,
        "usage": usage_wire(&usage),
    });
    match observe_model(
        host,
        ModelCapture {
            request_bytes,
            usage,
            finish_reason: finish_reason.to_owned(),
            response: &response,
            latency_ms,
            record,
        },
        last_content,
    ) {
        Ok(()) => EngineOutcome::Failed {
            reason: FailureReason::ProviderError,
            partial: partial_report(last_content),
        },
        Err(outcome) => outcome,
    }
}

/// 执行单个工具调用：解析 JSON、调用只读工具、写 blob 与观测、返回工具结果。
///
/// **作用**：结果正文进入下一轮 `Transcript`，引用进入工具观测。
/// **参数**：`call.arguments` 在进入本函数前已是合法 JSON（传输层保证）。`last_content` 只在失败时用作部分报告。`cancel` 在工具执行期间触发时立即返回 `Cancelled`。
/// **边界**：未知工具名仍 observe，正文写明 unknown，不中断 Loop。工具 I/O 失败同样写成结果正文并继续。
/// 同步的文件系统工作放进 `spawn_blocking`，不占住异步 worker。
/// 阻塞线程里的遍历会自己跑完，结果被丢弃。
async fn run_one_tool(
    host: &Arc<dyn HostPort>,
    workspace: &WorkspaceRoot,
    call: &ToolCall,
    last_content: &str,
    cancel: &CancellationToken,
) -> Result<ContextToolResult, EngineOutcome> {
    let started = Instant::now();
    let (workspace_owned, call_owned) = (workspace.clone(), call.clone());
    let task = tokio::task::spawn_blocking(move || dispatch_tool(&workspace_owned, &call_owned));
    let joined = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(EngineOutcome::Cancelled {
                partial: partial_report(last_content),
            });
        }
        joined = task => joined,
    };
    let Ok((path_or_query, rendered, truncated)) = joined else {
        return Err(EngineOutcome::Failed {
            reason: FailureReason::EngineError,
            partial: partial_report(last_content),
        });
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let result_ref = put_bytes(host, rendered.as_bytes(), "text/plain", last_content)?;
    let byte_count = rendered.len() as u64;
    observe(
        host,
        EngineObservation::ToolCall {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            normalized_path_or_query: path_or_query,
            result_ref: result_ref.clone(),
            byte_count,
            truncated,
            duration_ms,
        },
        last_content,
    )?;
    Ok(ContextToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        result_ref: result_ref.as_str().to_owned(),
        byte_count,
        body: rendered,
    })
}

/// 按工具名解析参数并调用，或生成未知工具正文。
///
/// **参数**：`call.name` 为 `list_files` / `read_file` / `search_code` 之一时走真实工具；其余为未知。
/// **边界**：字段名固定为 `path` / `page` / `start_line` / `query` / `offset`。缺省（字段不存在或为 null）：`page`/`start_line`=1，`offset`=0，`path` 空串表示根。
/// 字段存在但类型或范围不对时不套默认值，正文写明哪个参数非法，让模型自己改。
fn dispatch_tool(workspace: &WorkspaceRoot, call: &ToolCall) -> (String, String, bool) {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    match call.name.as_str() {
        "list_files" => {
            let parsed = str_arg(&args, "path")
                .and_then(|path| u32_arg(&args, "page", 1).map(|page| (path, page)));
            let (path, page) = match parsed {
                Ok(parsed) => parsed,
                Err(key) => return invalid_argument_text(&call.name, key),
            };
            match list_files(workspace, path, page) {
                Ok(output) => from_output(output),
                Err(error) => tool_error_text(&call.name, path_label(path), &error),
            }
        }
        "read_file" => {
            let parsed = str_arg(&args, "path").and_then(|path| {
                u32_arg(&args, "start_line", 1).map(|start_line| (path, start_line))
            });
            let (path, start_line) = match parsed {
                Ok(parsed) => parsed,
                Err(key) => return invalid_argument_text(&call.name, key),
            };
            match read_file(workspace, path, start_line) {
                Ok(output) => from_output(output),
                Err(error) => tool_error_text(&call.name, path_label(path), &error),
            }
        }
        "search_code" => {
            let parsed = str_arg(&args, "query")
                .and_then(|query| u32_arg(&args, "offset", 0).map(|offset| (query, offset)));
            let (query, offset) = match parsed {
                Ok(parsed) => parsed,
                Err(key) => return invalid_argument_text(&call.name, key),
            };
            match search_code(workspace, query, offset) {
                Ok(output) => from_output(output),
                Err(error) => tool_error_text(
                    &call.name,
                    if query.is_empty() { "query" } else { query },
                    &error,
                ),
            }
        }
        other => {
            let body = format!("unknown tool: {other}\n");
            (other.to_owned(), body, false)
        }
    }
}

/// 取字符串参数。缺失或 null 为空串；存在但不是字符串返回字段名。
fn str_arg<'a>(args: &'a Value, key: &'static str) -> Result<&'a str, &'static str> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(""),
        Some(value) => value.as_str().ok_or(key),
    }
}

/// 取非负整数参数。缺失或 null 用 `default`；存在但不是能放进 `u32` 的非负整数返回字段名。
fn u32_arg(args: &Value, key: &'static str, default: u32) -> Result<u32, &'static str> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(key),
    }
}

/// 参数非法时的工具结果正文。`normalized_path_or_query` 退回工具名，保持非空。
fn invalid_argument_text(tool_name: &str, key: &str) -> (String, String, bool) {
    (
        tool_name.to_owned(),
        format!("tool: {tool_name}\nerror: invalid argument `{key}`\n"),
        false,
    )
}

/// 从成功的 `ToolOutput` 取出观察字段与渲染正文。
fn from_output(output: ToolOutput) -> (String, String, bool) {
    let path = output.normalized_path_or_query.clone();
    let truncated = output.truncated;
    let rendered = output.render();
    (path, rendered, truncated)
}

/// 工具失败时仍要有非空 `normalized_path_or_query` 和可读正文。
fn tool_error_text(
    tool_name: &str,
    path_or_query: &str,
    error: &ToolError,
) -> (String, String, bool) {
    let label = if path_or_query.is_empty() {
        tool_name
    } else {
        path_or_query
    };
    (
        label.to_owned(),
        format!("tool: {tool_name}\nerror: {error}\n"),
        false,
    )
}

/// 空路径在观察里不能用空串，改成 `.`。
fn path_label(path: &str) -> &str {
    if path.is_empty() {
        "."
    } else {
        path
    }
}

/// 声明三个只读工具的 schema。
///
/// **边界**：与 `dispatch_tool` 字段名一致。变更时递增 [`PROMPT_LAYOUT_VERSION`]。
fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "list_files".to_owned(),
            description: "List one page of entries under a workspace-relative directory."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative directory; empty or omit for root." },
                    "page": { "type": "integer", "minimum": 1, "description": "1-based page." }
                }
            }),
        },
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read numbered lines from a workspace-relative text file.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "search_code".to_owned(),
            description: "Search the workspace with a regular expression.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 }
                },
                "required": ["query"]
            }),
        },
    ]
}

/// 把观测值收成历史里的字符串正文。
fn observed_as_string(text: &ObservedText) -> String {
    match text {
        ObservedText::Text(value) => value.clone(),
        ObservedText::Empty | ObservedText::Absent => String::new(),
    }
}

/// 非空 content 才带部分报告。
fn partial_report(content: &str) -> Option<ArtifactDraft> {
    if content.is_empty() {
        None
    } else {
        Some(ArtifactDraft::report(content.to_owned()))
    }
}

/// `put_blob`；存储失败收成 `EngineError`。
fn put_bytes(
    host: &Arc<dyn HostPort>,
    bytes: &[u8],
    media_type: &str,
    last_content: &str,
) -> Result<fluxaxiom_engine_api::BlobRef, EngineOutcome> {
    host.put_blob(Bytes::copy_from_slice(bytes), media_type)
        .map_err(|error| host_fail(error, last_content))
}

/// `observe`；校验或存储失败收成对应终态。
fn observe(
    host: &Arc<dyn HostPort>,
    observation: EngineObservation,
    last_content: &str,
) -> Result<(), EngineOutcome> {
    host.observe(observation)
        .map_err(|error| host_fail(error, last_content))
}

/// Host 端口错误统一收尾。`BudgetExhausted` 不经过这里。
fn host_fail(error: HostPortError, last_content: &str) -> EngineOutcome {
    let reason = match error {
        HostPortError::Invalid | HostPortError::Unsupported | HostPortError::Storage => {
            FailureReason::EngineError
        }
    };
    EngineOutcome::Failed {
        reason,
        partial: partial_report(last_content),
    }
}

/// Provider `finish_reason` 写入观测的文本。
///
/// **边界**：`Other` 用原文，保证非空。空 `Other` 回落为 `other`。
fn finish_reason_text(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_owned(),
        FinishReason::Length => "length".to_owned(),
        FinishReason::ToolCalls => "tool_calls".to_owned(),
        FinishReason::Other(text) if text.is_empty() => "other".to_owned(),
        FinishReason::Other(text) => text.clone(),
    }
}

/// 响应 blob 的 JSON 形状。含思维链与工具调用原文，事件里只有引用。
fn completion_wire(completion: &ChatCompletion) -> Value {
    json!({
        "reasoning_content": observed_json(&completion.reasoning),
        "content": observed_json(&completion.content),
        "tool_calls": completion.tool_calls.iter().map(|call| json!({
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        })).collect::<Vec<_>>(),
        "finish_reason": finish_reason_text(&completion.finish_reason),
        "usage": usage_wire(&completion.usage),
        "prompt_layout_version": PROMPT_LAYOUT_VERSION,
    })
}

fn observed_json(text: &ObservedText) -> Value {
    match text {
        ObservedText::Absent => Value::Null,
        ObservedText::Empty => Value::String(String::new()),
        ObservedText::Text(value) => Value::String(value.clone()),
    }
}

fn usage_wire(usage: &ModelUsage) -> Value {
    json!({
        "prompt_tokens": token_json(usage.prompt_tokens),
        "completion_tokens": token_json(usage.completion_tokens),
        "prompt_cache_hit_tokens": token_json(usage.prompt_cache_hit_tokens),
        "prompt_cache_miss_tokens": token_json(usage.prompt_cache_miss_tokens),
    })
}

fn token_json(count: fluxaxiom_contract::TokenCount) -> Value {
    match count {
        fluxaxiom_contract::TokenCount::Known(n) => json!(n),
        fluxaxiom_contract::TokenCount::Unknown => Value::String("unknown".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::TokenCount;

    /// 构造四项都已知的用量。
    ///
    /// **作用**：视图序列化测试少写重复字段。
    /// **参数**：`prompt` 与 `completion` 写入对应的已知 token 数，0 允许。
    /// **边界**：缓存命中与未命中固定为 0。不表示未知。
    fn usage(prompt: u64, completion: u64) -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(prompt),
            completion_tokens: TokenCount::Known(completion),
            prompt_cache_hit_tokens: TokenCount::Known(0),
            prompt_cache_miss_tokens: TokenCount::Known(0),
        }
    }

    /// 恒等视图收成的请求与手写 `ChatRequest` 逐字段相同。
    #[test]
    fn identity_view_matches_the_hand_built_chat_request() {
        let hand = vec![
            Message::System {
                content: SYSTEM_PROMPT.to_owned(),
            },
            Message::User {
                content: "task".to_owned(),
            },
            Message::Assistant {
                content: "look".to_owned(),
                reasoning_content: "think".to_owned(),
                tool_calls: vec![ToolCall {
                    id: "c1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"a"}"#.to_owned(),
                }],
            },
            Message::Tool {
                content: "file body".to_owned(),
                tool_call_id: "c1".to_owned(),
            },
        ];
        let view = View {
            system: SYSTEM_PROMPT.to_owned(),
            user: "task".to_owned(),
            rounds: vec![ContextRound {
                assistant: AssistantTurn {
                    content: "look".to_owned(),
                    reasoning: "think".to_owned(),
                    tool_calls: vec![ContextToolCall {
                        id: "c1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: r#"{"path":"a"}"#.to_owned(),
                    }],
                },
                tools: vec![ContextToolResult {
                    call_id: "c1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"a"}"#.to_owned(),
                    result_ref: "sha256:abc".to_owned(),
                    byte_count: 9,
                    body: "file body".to_owned(),
                }],
                usage: usage(10, 4),
                trailing_user: String::new(),
            }],
        };
        let from_view =
            ChatRequest::new(DEFAULT_MODEL, messages_from_view(&view)).with_tools(tool_specs());
        let hand_built = ChatRequest::new(DEFAULT_MODEL, hand).with_tools(tool_specs());
        assert_eq!(
            serde_json::to_value(&from_view).expect("view request"),
            serde_json::to_value(&hand_built).expect("hand request")
        );
    }
}
