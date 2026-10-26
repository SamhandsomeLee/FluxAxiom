//! DeepSeek Chat Completions 客户端与 Engine A 只读 Loop。
//!
//! **作用**：固定请求里的 `reasoning_content`、缓存用量和流式 `tool_calls` 字段；`EngineA` 经 `HostPort` 跑最小 Agent Loop。
//! **边界**：不写 Host 的 event 或 blob（由 `HostPort` 实现负责）。只有 `DeepSeekClient::from_env` 读取 `DEEPSEEK_API_KEY`。
//! 默认测试用 Fake，不访问网络。`ChatTransport::round` 与 `execute` 是异步的，取消令牌与 `HostPort::cancellation` 使用同一类型。
//! `ChatRequest::new` 默认带未校准的 `max_tokens: 32768`。

mod chat;
mod doc_version;
mod engine;
mod fake;
mod http;
mod rebuild;
mod retry;
mod sse;
mod tools;

pub use chat::{
    observe_reasoning, replay_reasoning, usage_from_json, ChatCompletion, ChatError, ChatRequest,
    FinishReason, Message, ObservedText, ReasoningEffort, ReplayReasoning, ToolCall, ToolSpec,
    API_KEY_ENV, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, MAX_TOKENS_LIMIT,
};
pub use doc_version::{lookup as lookup_doc_version, DocVersion};
pub use engine::{EngineA, PROMPT_LAYOUT_VERSION};
pub use fake::{FakeChat, FakeStep, ScriptedTurn};
pub use fluxaxiom_contract::{ModelUsage, RebuildResult, TokenCount};
pub use fluxaxiom_engine_api::{WorkspaceError, WorkspaceRoot};
pub use http::{stream_request_body, DeepSeekClient, DEFAULT_BASE_URL};
pub use rebuild::{format_rebuild_line, verify as verify_rebuild, RebuildCall};
pub use retry::{
    execute, Backoff, ChatTransport, RecordingBackoff, Round, SleepBackoff, RETRY_DELAYS,
};
pub use sse::parse_sse;
pub use tokio_util::sync::CancellationToken;
pub use tools::{
    list_files, read_file, search_code, ToolError, ToolOutput, LIST_PAGE_LIMIT, READ_BYTE_LIMIT,
    READ_LINE_LIMIT, SEARCH_MATCH_LIMIT,
};
