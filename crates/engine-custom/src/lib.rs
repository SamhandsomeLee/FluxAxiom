//! DeepSeek Chat Completions 的薄客户端契约。
//!
//! **作用**：固定请求里的 `reasoning_content` 和缓存用量字段，并用 Fake 覆盖失败场景。
//! **边界**：不实现 `EngineAdapter`，不写 Host 的 event 或 blob。
//! 只有 `DeepSeekClient::from_env` 读取 `DEEPSEEK_API_KEY`。默认测试不访问网络。

mod chat;
mod fake;
mod http;
mod retry;
mod sse;

pub use chat::{
    observe_reasoning, replay_reasoning, usage_from_json, ChatCompletion, ChatError, ChatRequest,
    FinishReason, Message, ObservedText, ReasoningEffort, ReplayReasoning, ToolCall, ToolSpec,
    API_KEY_ENV, DEFAULT_MODEL,
};
pub use fake::{FakeChat, FakeStep, ScriptedTurn};
pub use fluxaxiom_contract::{ModelUsage, TokenCount};
pub use http::{stream_request_body, DeepSeekClient, DEFAULT_BASE_URL};
pub use retry::{execute, Backoff, ChatTransport, RecordingBackoff, Round, RETRY_DELAYS};
pub use sse::parse_sse;
