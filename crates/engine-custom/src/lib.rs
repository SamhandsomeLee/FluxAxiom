//! DeepSeek Chat Completions 的薄客户端契约。
//!
//! **作用**：固定请求里的 `reasoning_content` 和缓存用量字段，并用 Fake 覆盖失败场景。
//! **边界**：不实现 `EngineAdapter`，不写 Host 的 event 或 blob。
//! 只有 `DeepSeekClient::from_env` 读取 `DEEPSEEK_API_KEY`。默认测试不访问网络。
//! `ChatTransport::round` 与 `execute` 是异步的，取消令牌与 `HostPort::cancellation` 使用同一类型。
//! 取消返回 `ChatError::Cancelled`，与网络中断 `StreamInterrupted` 分开。

mod chat;
mod fake;
mod http;
mod retry;
mod sse;
mod tools;

pub use chat::{
    observe_reasoning, replay_reasoning, usage_from_json, ChatCompletion, ChatError, ChatRequest,
    FinishReason, Message, ObservedText, ReasoningEffort, ReplayReasoning, ToolCall, ToolSpec,
    API_KEY_ENV, DEFAULT_MODEL,
};
pub use fake::{FakeChat, FakeStep, ScriptedTurn};
pub use fluxaxiom_contract::{ModelUsage, TokenCount};
pub use fluxaxiom_engine_api::{WorkspaceError, WorkspaceRoot};
pub use http::{stream_request_body, DeepSeekClient, DEFAULT_BASE_URL};
pub use retry::{
    execute, Backoff, ChatTransport, RecordingBackoff, Round, SleepBackoff, RETRY_DELAYS,
};
pub use sse::parse_sse;
pub use tokio_util::sync::CancellationToken;
pub use tools::{
    list_files, read_file, search_code, ToolError, ToolOutput, LIST_PAGE_LIMIT, READ_BYTE_LIMIT,
    READ_LINE_LIMIT, SEARCH_MATCH_LIMIT,
};
