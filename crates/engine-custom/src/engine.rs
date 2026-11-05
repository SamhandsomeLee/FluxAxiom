//! Engine A：最小只读 Agent Loop。
//!
//! **作用**：实现 `EngineAdapter::run`。Host 在 Attempt 进入 `Running` 后调用。按预算预留模型与工具调用，串起 Chat Completions 与三个只读工具，经 `HostPort` 写 blob 与观测。
//! **边界**：不写 SQLite，不发 `run.*` / `attempt.*`，不写工作区，不调用 shell。密钥、`reasoning_content`、prompt 与文件全文只进 blob，不进观测 payload。默认测试用 Fake，不拨号。

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

/// 布局版本字面量的唯一来源。
///
/// **作用**：`concat!` 只接受字面量，宏同时喂给 [`PROMPT_LAYOUT_VERSION`] 和 [`SYSTEM_PROMPT`]。
/// **边界**：展开结果是非空字符串。改这里就是改对外的提示布局版本。
macro_rules! layout_version {
    () => {
        "p0a-loop-1"
    };
}

/// 写入响应 blob 与样本记录的布局版本。
///
/// **作用**：标识 system 文本和工具 schema。样本靠它识别提示布局。
/// **边界**：当前字面量是 `p0a-loop-1`。system 或工具 schema 变更时递增。不是模型名，也不是 crate 版本。
pub const PROMPT_LAYOUT_VERSION: &str = layout_version!();

/// Engine 身份。
///
/// **作用**：`descriptor` 交给 Host，写入 Attempt 的 `engine_id`。
/// **边界**：固定为 `engine-a`，非空。不是权限声明。
const ENGINE_ID: &str = "engine-a";

/// 实现版本。
///
/// **作用**：`descriptor` 交给 Host，写入 Attempt 的 `engine_version`。
/// **边界**：取本 crate 的 `CARGO_PKG_VERSION`，不是 git 提交号。
const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 固定在消息列表最前的 system 文本。
///
/// **作用**：`run` 放进事实的 system，再成为发给模型的第一条消息。
/// **边界**：正文含 [`PROMPT_LAYOUT_VERSION`]。改措辞或工具约定时要递增该版本。不是密钥；会进入请求 blob，不单独进观测 payload。
const SYSTEM_PROMPT: &str = concat!(
    "You are FluxAxiom Engine A in read-only analysis mode (prompt_layout_version=",
    layout_version!(),
    ").\n",
    "Use only the provided tools to inspect the authorized workspace.\n",
    "Do not invent file contents. When finished, answer in plain text without further tool calls.\n",
);

/// 最小只读 Agent Loop。
///
/// **作用**：持有可替换的 Chat 传输与退避策略，实现 `EngineAdapter`。CLI 或测试用 [`EngineA::new`] 构造。Host 用 `descriptor` 识别它，用 `run` 驱动它。
/// **边界**：`transport` 与 `backoff` 在 `run` 期间独占。同一实例已有 `run` 在跑时，后到的 `run` 立刻 `Failed(EngineError)`，不排队。
/// `ChatTransport + Send` 与 `Backoff + Send` 只约束 trait 实现，`new` 不检查。本类型不实现 `Debug`。
pub struct EngineA<T, B> {
    /// 一次 Chat 请求的发送端。`run` 用 `try_lock` 独占，拿不到锁就拒绝这次调用。密钥留在 `T` 里；本结构没有 `Debug`，也不会把传输层打印出来。
    transport: Mutex<T>,
    /// 429 / 5xx 的等待策略。`run` 必须和 `transport` 一起锁住才开始。间隔由 `B` 决定，测试里的实现可以不睡眠。
    backoff: Mutex<B>,
}

impl<T, B> EngineA<T, B> {
    /// 注入传输层与退避策略。
    ///
    /// **作用**：CLI 或测试构造一个还没在跑的 `EngineA`。本函数不发起请求。
    /// **参数**：`transport` 被移入内部锁，负责发送一次已序列化的 Chat 请求。`backoff` 被移入另一把锁，只负责 429 / 5xx 的等待，不发送请求。
    /// **边界**：不读 `DEEPSEEK_API_KEY`，也不检查这两个值能否 `Send`。真实 DeepSeek 客户端由调用方构造后传入。两把锁一开始都空闲。
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
    /// 回报实现身份。
    ///
    /// **作用**：Host 把返回值写入 Attempt 的 `engine_id` 与 `engine_version`。
    /// **边界**：固定为 `engine-a` 和本 crate 的包版本。不是权限声明，也不读取本次任务。
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: ENGINE_ID.to_owned(),
            version: ENGINE_VERSION.to_owned(),
        }
    }

    /// 在已授权工作区上跑只读分析。
    ///
    /// **作用**：Host 在 Attempt 进入 `Running` 之后调用。经 `host` 预留预算、写 blob 和观测，把 `EngineOutcome` 交回 Host 收尾。
    /// **参数**：`request` 由调用方移入。`input` 成为首条 user 消息，空串照发，这里不拒绝；`workspace` 必须已由 Host `open`，工具只在这棵根里读；本实现不读 `run_id`。
    /// `host` 是预算、blob、观测和取消令牌的唯一入口，整次 `run` 共享这一个 `Arc`。不调用 `propose_effect` 或 `request_input`。
    /// **边界**：同一实例上已有 `run` 时立刻 `Failed(EngineError)`，不排队。档案解析失败同样是 `EngineError`，不是 `ContextExhausted`。
    /// 每轮先 `prepare`。超硬上限时先保存记录并 `observe` `context.exhausted`，再 `Failed(ContextExhausted)`，不占模型额度。保存或观测失败则 `Failed(EngineError)`，不能把诊断丢失当成已记录的耗尽。
    /// 通过后预留 1 次模型、0 次工具。一批 N 个工具前预留 0 次模型、N 次工具。预留失败是 `Failed(BudgetExhausted)`。预留成功后，后面的失败不归还额度。
    /// `execute` 内部的 429 / 5xx 重试不再预留。`latency_ms` 含这些退避。预留之后，请求序列化失败、缺密钥或无法编码是 `Failed(EngineError)`。HTTP 失败、流中断、连接失败是 `Failed(ProviderError)`。这几类都不写 `ModelCall`：没有可信用量和结束原因。Host 上的模型调用数可以小于已预留数。是否补写 `usage=Unknown` 留待 EA-02。
    /// `finish_reason` 为 `length` 时 `Failed(BudgetExhausted)`，不执行工具。其余情况下只要带工具调用就执行，包括 `stop`。畸形或非法工具 JSON 先 `observe` 再 `Failed(ProviderError)`。未知工具名 `observe` 后继续；工具 I/O 失败写成结果正文并继续。
    /// 没有工具调用且结束原因是 `stop` 或 `tool_calls` 时，非空正文成为成功报告；正文为空则 `Failed(ProviderError)`。结束原因是其他值且没有工具调用时，也是 `Failed(ProviderError)`。失败或取消的 partial 是迄今最后一段非空 assistant 正文，本轮正文为空时不擦掉更早的文字。
    /// 取消在每轮模型前、`execute` 期间、每个工具前，以及工具执行期间都生效，返回 `Cancelled`。`execute` 因取消返回时不写 `ModelCall`。阻塞线程里已经开始的遍历会自己跑完，结果丢弃。
    /// 不写 SQLite，不发 `run.*` / `attempt.*`，不调用 shell。模型别名固定为 [`DEFAULT_MODEL`]，输出上限固定为 [`DEFAULT_MAX_TOKENS`]。密钥、思维链、prompt 和文件全文只进 blob。
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
/// **作用**：`run` 在进入循环前调用一次，得到这次 `prepare` 用的 `ContextManager`。
/// **边界**：档案文本无法解析时返回 `ProfileError`。调用方收成 `EngineError`，不是 `ContextExhausted`。不访问网络或磁盘。
fn deepseek_manager() -> Result<ContextManager, fluxaxiom_context::ProfileError> {
    Ok(ContextManager::new(fluxaxiom_context::deepseek_flash()?))
}

/// 保存 prepare 拒绝发送请求时的诊断记录。
///
/// **作用**：`run` 在 `prepare` 返回耗尽时调用。把失败记录的 blob 引用经 `ContextExhausted` 观测关联到 Run，不伪造模型调用。
/// **参数**：`host` 接收一次记录 blob 和一次诊断观测。`record` 是失败 `prepare` 返回的记录，只有数值和引用，不含正文。`last_content` 是已有的 assistant 正文，空串表示没有部分报告。
/// **边界**：不预留模型额度，不发送请求。存储或观测失败时返回 `Failed(EngineError)`，并带上部分报告。`Ok` 只表示诊断已提交。
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
/// **作用**：`observe_context_exhausted` 与 `observe_model` 共用这条写入路径。调用方再决定要不要写观测。
/// **参数**：`host` 保存序列化后的字节。`record` 被序列化成 JSON，只含数值与引用，不含 prompt 或工具正文。`last_content` 只在失败时用作部分报告，空串表示没有。
/// **边界**：序列化或存储失败时返回 `Failed(EngineError)`，不静默丢弃记录。媒体类型是 `application/json`。本函数不写观测，也不检查记录能不能重放。
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
/// **作用**：`run` 在 `prepare` 成功后交给 `ChatRequest`。本模块测试用它和手写请求对照。
/// **参数**：`view` 只被借用，字符串会克隆进新消息。`system` 与首条 `user` 原样放入，空串也发送，这里不拒绝。各轮 assistant 使用正文、思维链和工具调用。工具结果只使用正文和调用 ID。
/// **边界**：`trailing_user` 为空串时不另起 user 消息。工具名、参数和 `result_ref` 不进入 tool 消息。空的工具调用列表由 `Message` 省略。思维链会进入随后的请求 blob。
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
/// **作用**：`observe_model` 的入参。`run` 与 `fail_after_usage_observe` 构造它，把请求字节、响应 JSON 和 context 记录收在一起。
/// **边界**：请求和响应可以含 prompt 与思维链，只进 blob。`record` 不含正文。本结构不实现 `Debug`，也不写日志。
struct ModelCapture<'a> {
    /// 已序列化的 Chat 请求，UTF-8 JSON。只借用调用方的缓冲区。含 system、用户任务和工具 schema，不进事件 payload。调用方在序列化成功后传入，这里不检查是否为空。
    request_bytes: &'a [u8],
    /// 这次响应的用量。未知项保持 `TokenCount::Unknown`，不用 0 冒充。四项都会进入 `ModelCall` 观测。
    usage: ModelUsage,
    /// 写入 `ModelCall.finish_reason` 的文本。调用方保证非空；空串会被 Host 判为 `Invalid`。
    finish_reason: String,
    /// 响应 blob 的 JSON。成功路径可含思维链、正文和工具参数；畸形路径只有结束标记和用量。原样进 blob，不进事件 payload。
    response: &'a Value,
    /// 这次 `execute` 的墙钟毫秒，含 429 / 5xx 退避。0 表示不足 1 毫秒，不是未知。
    latency_ms: u64,
    /// 发出这次请求前的 `prepare` 记录。只借用。只有数值和引用，不含 prompt 或工具正文。
    record: &'a fluxaxiom_context::ContextRecord,
}

/// 把一次模型响应写成 blob 并 observe。
///
/// **作用**：`run` 在 `execute` 成功后调用；`fail_after_usage_observe` 在畸形或非法参数时调用。请求、响应和 context 记录各自落 blob，观测里只留引用。
/// **参数**：`host` 接收三次 `put_blob` 和一次 `ModelCall` 观测。`capture` 是这次要落盘的请求、用量、结束原因、响应 JSON 和 prepare 记录；结束原因必须非空。`last_content` 只在失败时用作部分报告，空串表示没有。
/// **边界**：序列化、存储或观测失败时返回已构造的 `EngineOutcome`，调用方直接返回。已写入的 blob 不删除。请求、响应和记录都用 `application/json`。不在这里重试，也不预留预算。
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

/// 畸形或非法 arguments 时先记用量，再以 Provider 错误结束。
///
/// **作用**：`run` 在 `ChatError::MalformedToolCall` 与 `InvalidToolArguments` 上调用。这次请求已经发出，记录与请求、响应一起提交。
/// **参数**：`host` 接收 blob 与 `ModelCall` 观测。`request_bytes` 是已序列化的请求 JSON，含 prompt，只进 blob。`usage` 是这次已计费的用量，未知项保持 `Unknown`。
/// `finish_reason` 是写入观测的非空标记，不是 Provider 原文；空串会被 Host 拒绝。`latency_ms` 是这次调用的墙钟毫秒，含退避，0 允许。
/// `last_content` 只在失败时用作部分报告，空串表示没有。`record` 是发出这次请求前的 `prepare` 记录，不含正文。
/// **边界**：响应 blob 只含结束标记与用量，不含半截工具调用。观测成功则返回 `Failed(ProviderError)`；观测或存储失败则返回那次失败的 `EngineOutcome`。
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

/// 执行单个工具调用，并返回要写进下一轮事实的结果。
///
/// **作用**：`run` 对每个已预留的工具调用一次。结果正文进入下一轮 `Transcript`，引用进入工具观测。
/// **参数**：`host` 写结果 blob 和 `ToolCall` 观测。`workspace` 是 Host 已 `open` 的授权根，工具不能走出这棵根。
/// `call` 的 `id` 与 `name` 原样进入观测。传输层保证成功完成的调用里二者非空，本函数不再检查；空值会让随后的 `observe` 被 Host 拒绝。`arguments` 在进入本函数前已是合法 JSON，空对象允许，字段含义由 `dispatch_tool` 解释。
/// `last_content` 只在失败时用作部分报告，空串表示没有。`cancel` 在工具执行期间触发时立即返回 `Cancelled`。
/// **边界**：未知工具名仍 observe，正文写明 unknown，不中断 Loop。工具 I/O 失败和参数类型不对同样写成结果正文并继续。
/// 同步的文件系统工作放进 `spawn_blocking`。取消与工具同时就绪时优先取消。阻塞线程里已经开始的遍历会自己跑完，结果被丢弃。`spawn_blocking` 自身失败返回 `Failed(EngineError)`。
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
/// **作用**：`run_one_tool` 在阻塞线程里调用。返回 `(路径或查询, 正文, 是否截断)`，供观测和下一轮事实使用。
/// **参数**：`workspace` 是授权根，三个真实工具都用它解析路径。`call.name` 为 `list_files`、`read_file`、`search_code` 时走对应工具；`arguments` 是 JSON 文本，解析失败当成 JSON `null`。解析成功但不是对象时，字段读取与缺字段相同。其他名字视为未知工具。
/// **边界**：字段名固定为 `path`、`page`、`start_line`、`query`、`offset`。字段不存在或为 `null` 时，`page` 与 `start_line` 为 1，`offset` 为 0，`path` 空串表示根。
/// 字段存在但不是字符串，或不是能放进 `u32` 的非负整数时，不套默认值，正文写明哪个参数非法。`page` 或 `start_line` 为 0 能通过解析，由工具返回 `InvalidRequest`。
/// 不调用 shell，不写文件。未知工具把 `name` 当作观测里的路径或查询；空名字会让随后的 `observe` 被 Host 拒绝。
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

/// 从工具参数对象里取一个字符串字段。
///
/// **作用**：`dispatch_tool` 读 `path` 与 `query`。
/// **参数**：`args` 是已解析的参数。不是对象时取字段得到 `None`，与缺字段相同。`key` 是字段名；类型不对时原样作为错误返回，调用方写进正文。
/// **边界**：字段缺失或 JSON `null` 返回空串，不报错。空字符串也合法。其他 JSON 类型返回 `Err(key)`，不改写字段值。
fn str_arg<'a>(args: &'a Value, key: &'static str) -> Result<&'a str, &'static str> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(""),
        Some(value) => value.as_str().ok_or(key),
    }
}

/// 从工具参数对象里取一个能放进 `u32` 的非负整数。
///
/// **作用**：`dispatch_tool` 读 `page`、`start_line`、`offset`。
/// **参数**：`args` 与 [`str_arg`] 相同。`key` 是字段名，类型或数值放不进 `u32` 时原样返回。`default` 只在字段缺失或为 `null` 时使用；页码和起始行传 1，搜索偏移传 0。
/// **边界**：存在但不是整数、为负、大于 `u32::MAX`，或带小数点、指数（包括 `1.0`）时返回 `Err(key)`，不用 `default`。0 是合法的解析结果，语义是否拒绝由工具决定。
fn u32_arg(args: &Value, key: &'static str, default: u32) -> Result<u32, &'static str> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(key),
    }
}

/// 参数类型不合法时的工具结果。
///
/// **作用**：`dispatch_tool` 在 [`str_arg`] 或 [`u32_arg`] 失败时返回，不调用文件系统。三元组是路径或查询、正文、截断标记。
/// **参数**：`tool_name` 写入正文，并作为观测用的路径或查询，因此必须非空，否则随后的 `observe` 会被 Host 拒绝。`key` 是非法字段名，原样放进正文。
/// **边界**：截断标记恒为 `false`。不访问磁盘。正文不含文件内容。
fn invalid_argument_text(tool_name: &str, key: &str) -> (String, String, bool) {
    (
        tool_name.to_owned(),
        format!("tool: {tool_name}\nerror: invalid argument `{key}`\n"),
        false,
    )
}

/// 从成功的工具输出取出观测字段和渲染正文。
///
/// **作用**：`dispatch_tool` 在三个工具返回 `Ok` 时调用。三元组是路径或查询、正文、截断标记。
/// **参数**：`output` 被移走。路径或查询、截断标记来自它的字段；正文是 `render()` 的纯文本，含工具头和截断说明。
/// **边界**：不重新判断截断。渲染文本会进入下一轮 tool 消息和结果 blob，不进事件 payload。本函数不打印它。
fn from_output(output: ToolOutput) -> (String, String, bool) {
    let path = output.normalized_path_or_query.clone();
    let truncated = output.truncated;
    let rendered = output.render();
    (path, rendered, truncated)
}

/// 工具执行失败时的结果三元组。
///
/// **作用**：`dispatch_tool` 在 `list_files`、`read_file`、`search_code` 返回 `Err` 时调用。Loop 把它当成普通工具结果继续。
/// **参数**：`tool_name` 写入正文。`path_or_query` 是路径或查询；空串时观测字段改用 `tool_name`，避免空串被 Host 拒绝。`error` 只贡献 `Display` 里的类别，不含路径和文件正文。
/// **边界**：截断标记恒为 `false`。不把 `ToolError` 变成 `EngineOutcome`。
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

/// 给失败路径一个非空标签。
///
/// **作用**：`list_files` 与 `read_file` 出错时，把标签交给 [`tool_error_text`]。
/// **参数**：`path` 是模型给出的相对路径。空串表示根，这里改成 `.`；其他字符串原样返回。
/// **边界**：不访问磁盘，也不把 `..` 判成越界。越界由工具自己返回。
fn path_label(path: &str) -> &str {
    if path.is_empty() {
        "."
    } else {
        path
    }
}

/// 声明三个只读工具的 schema。
///
/// **作用**：`run` 每一轮都把这份声明放进 `ChatRequest`。测试用它和手写请求对照。
/// **边界**：字段名与 [`dispatch_tool`] 一致。`list_files` 的 `path` 可省略；`read_file` 要求 `path`；`search_code` 要求 `query`。变更 schema 或说明时递增 [`PROMPT_LAYOUT_VERSION`]。不声明写文件或 shell。
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

/// 把观测到的文本收成历史里的字符串。
///
/// **作用**：`run` 用它取出 assistant 正文，再决定是否更新部分报告。`completion_wire` 不走这里。
/// **参数**：`text` 是响应里的一段观测文本。`Text` 克隆其内容；`Empty` 与 `Absent` 都变成空串。
/// **边界**：这里不再区分字段缺失和空字符串。空串不会覆盖已有部分报告，由调用方判断。正文不写日志。
fn observed_as_string(text: &ObservedText) -> String {
    match text {
        ObservedText::Text(value) => value.clone(),
        ObservedText::Empty | ObservedText::Absent => String::new(),
    }
}

/// 用已有正文构造可选的部分报告。
///
/// **作用**：`run` 在失败或取消时放进 `EngineOutcome` 的 `partial`。成功完成不走这里。
/// **参数**：`content` 是到目前为止最后一段非空 assistant 正文。空串返回 `None`，表示没有部分报告，不是一份空 Artifact。
/// **边界**：非空时走 `ArtifactDraft::report`。不在这里写 blob。
fn partial_report(content: &str) -> Option<ArtifactDraft> {
    if content.is_empty() {
        None
    } else {
        Some(ArtifactDraft::report(content.to_owned()))
    }
}

/// 把一段字节交给 Host 存成 blob。
///
/// **作用**：请求、响应、context 记录和工具正文共用。`observe_model`、`put_context_record`、`run_one_tool` 调用。
/// **参数**：`host` 执行 `put_blob`。`bytes` 是要保存的原文，空切片也原样交给 Host。`media_type` 是 `application/json` 或 `text/plain`，这里不改写；正式 Host 拒绝空串。`last_content` 只在存储失败时用作部分报告，空串表示没有。
/// **边界**：`Unsupported`、`Invalid`、`Storage` 都收成 `Failed(EngineError)`。成功时返回 `BlobRef`，不写观测。
fn put_bytes(
    host: &Arc<dyn HostPort>,
    bytes: &[u8],
    media_type: &str,
    last_content: &str,
) -> Result<fluxaxiom_engine_api::BlobRef, EngineOutcome> {
    host.put_blob(Bytes::copy_from_slice(bytes), media_type)
        .map_err(|error| host_fail(error, last_content))
}

/// 把一条观测交给 Host。
///
/// **作用**：`observe_model`、`observe_context_exhausted`、`run_one_tool` 在对应 blob 写好之后调用。
/// **参数**：`host` 执行 `observe`。`observation` 只含引用、用量和计数；空字符串或空 `BlobRef` 由 Host 判为 `Invalid`。`last_content` 只在失败时用作部分报告，空串表示没有。
/// **边界**：返回 `Ok` 时 Host 的 event 已提交。`Unsupported`、`Invalid`、`Storage` 都收成 `Failed(EngineError)`。预算耗尽不经过这里。
fn observe(
    host: &Arc<dyn HostPort>,
    observation: EngineObservation,
    last_content: &str,
) -> Result<(), EngineOutcome> {
    host.observe(observation)
        .map_err(|error| host_fail(error, last_content))
}

/// 把 Host 端口错误收成一次失败终态。
///
/// **作用**：[`put_bytes`] 与 [`observe`] 的错误映射。
/// **参数**：`error` 是 `put_blob` 或 `observe` 的失败。`Invalid`、`Unsupported`、`Storage` 都变成 `EngineError`，不保留端口错误文本。`last_content` 非空时放进 `partial`，空串则 `partial` 为 `None`。
/// **边界**：`BudgetExhausted` 不经过这里。预算失败由 `reserve` 的调用点直接返回。不重试。
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

/// 把结束原因写成观测和响应 blob 里的文本。
///
/// **作用**：`run` 填 `ModelCapture.finish_reason`，[`completion_wire`] 写入响应 JSON。
/// **参数**：`reason` 是这次完成响应的结束原因。`Stop`、`Length`、`ToolCalls` 写成 `stop`、`length`、`tool_calls`。`Other` 使用 Provider 原文。
/// **边界**：`Other` 的空串改写成 `other`，保证结果非空。原文只出现在返回值和随后的 blob 里。
fn finish_reason_text(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_owned(),
        FinishReason::Length => "length".to_owned(),
        FinishReason::ToolCalls => "tool_calls".to_owned(),
        FinishReason::Other(text) if text.is_empty() => "other".to_owned(),
        FinishReason::Other(text) => text.clone(),
    }
}

/// 组装成功响应的 blob JSON。
///
/// **作用**：`run` 在 [`observe_model`] 之前调用。事件只保存这份 JSON 的 blob 引用。
/// **参数**：`completion` 是 `execute` 的成功结果。思维链、正文、工具调用原文、结束原因、用量和 [`PROMPT_LAYOUT_VERSION`] 都写进返回值。
/// **边界**：不写密钥。畸形或非法工具参数不走这里。`Absent` 的思维链或正文写成 JSON `null`，空串写成 `""`。返回值不进日志。
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

/// 把观测文本收成响应 blob 里的 JSON 值。
///
/// **作用**：[`completion_wire`] 写 `reasoning_content` 与 `content`。
/// **参数**：`text` 是响应里的一段观测文本。`Absent` 变成 JSON `null`，`Empty` 变成空字符串，`Text` 克隆正文。
/// **边界**：不把缺失填成空串。正文会进入 blob，不进入事件 payload，这里也不打印它。
fn observed_json(text: &ObservedText) -> Value {
    match text {
        ObservedText::Absent => Value::Null,
        ObservedText::Empty => Value::String(String::new()),
        ObservedText::Text(value) => Value::String(value.clone()),
    }
}

/// 把四项用量收成响应 blob 里的 JSON 对象。
///
/// **作用**：[`completion_wire`] 与 [`fail_after_usage_observe`] 写入 `usage`。
/// **参数**：`usage` 的每一项单独交给 [`token_json`]。已知数保持 JSON 数字，`Unknown` 写成字符串 `unknown`。
/// **边界**：四个键都写出：`prompt_tokens`、`completion_tokens`、`prompt_cache_hit_tokens`、`prompt_cache_miss_tokens`。不省略未知项，也不用 0 表示未知。
fn usage_wire(usage: &ModelUsage) -> Value {
    json!({
        "prompt_tokens": token_json(usage.prompt_tokens),
        "completion_tokens": token_json(usage.completion_tokens),
        "prompt_cache_hit_tokens": token_json(usage.prompt_cache_hit_tokens),
        "prompt_cache_miss_tokens": token_json(usage.prompt_cache_miss_tokens),
    })
}

/// 把一项 token 计数收成 JSON。
///
/// **作用**：[`usage_wire`] 对四个字段各调用一次。
/// **参数**：`count` 为 `Known` 时返回该非负整数；为 `Unknown` 时返回字符串 `unknown`。0 是已知的零，不是未知。
/// **边界**：输入已经是 `TokenCount`，没有第三种取值。形状与该类型的序列化一致。
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
    /// **作用**：本模块的视图序列化测试少写重复字段。
    /// **参数**：`prompt` 写入已知的 prompt token 数，`completion` 写入已知的 completion token 数。二者都允许 0，0 是已知零。
    /// **边界**：缓存命中与未命中固定为 `Known(0)`，不表示未知。
    fn usage(prompt: u64, completion: u64) -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(prompt),
            completion_tokens: TokenCount::Known(completion),
            prompt_cache_hit_tokens: TokenCount::Known(0),
            prompt_cache_miss_tokens: TokenCount::Known(0),
        }
    }

    /// 恒等视图收成的请求与手写 `ChatRequest` 逐字段相同。
    ///
    /// **作用**：锁住 [`messages_from_view`] 对 system、user、assistant 思维链、工具调用和 tool 正文的映射。
    /// **边界**：夹具的 `trailing_user` 为空，断言里不会出现第二条 user。工具的 `result_ref` 不出现在请求 JSON 里。
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
