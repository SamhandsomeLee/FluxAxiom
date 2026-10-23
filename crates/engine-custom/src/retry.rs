use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatCompletion, ChatError, ChatRequest};

/// 429 / 5xx 的三次重试间隔。第一次失败后等 200ms，然后 400ms，然后 800ms。
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// 一次传输的结果。
///
/// **作用**：让重试逻辑和具体 HTTP 分开。
/// **边界**：400 不重试。429 与 5xx 最多重试 3 次。流中断、取消、非法工具参数，以及还没拿到状态码的 `Transport` 不重试。`summary` 只是截断后的错误说明，不是响应原文。
/// `Interrupted` 是网络侧的中断，`Cancelled` 是调用方触发了取消令牌，两者分开，Host 才能把前者记为失败、后者记为取消。
#[derive(Debug, PartialEq, Eq)]
pub enum Round {
    Completed(ChatCompletion),
    /// HTTP 状态。`summary` 只来自 JSON 的 `error.message` 或 `message`，最长 160 个字符。没有则为空，不放原始正文。
    Status {
        status: u16,
        summary: String,
    },
    Interrupted,
    /// 取消令牌已触发。不附带半截文本。
    Cancelled,
    InvalidToolArguments,
    /// 工具调用形状违约。与非法 JSON 一样不重试。
    MalformedToolCall,
}

/// 发送一次已序列化的请求。
///
/// **作用**：Fake 与 DeepSeek HTTP 客户端都走这个入口。
/// **边界**：实现不读密钥。请求 JSON 只用于协议判断，不写入日志。
/// `round` 是异步的。取消令牌在发送和读块时都要检查，触发后交回 `Cancelled`，不附带半截文本。
/// 要求 `Send`，这样 `execute` 的 future 才能放进 `EngineAdapter::run`。
#[async_trait]
pub trait ChatTransport: Send {
    /// **参数**：`request_json` 是已经序列化的请求。实现用它判断协议，不写入日志。
    /// `cancel` 在发送或读块期间被触发时，实现返回 `Cancelled`，不返回已拼片段。
    async fn round(
        &mut self,
        request_json: &Value,
        cancel: &CancellationToken,
    ) -> Result<Round, ChatError>;
}

/// 重试之间的等待。
///
/// **作用**：生产用 `SleepBackoff` 真正等待，测试可以只记录间隔，不真正睡觉。
/// **边界**：等待期间取消令牌被触发时返回 `Cancelled`，不再进入下一次 `round`。
/// 要求 `Send`，原因同 `ChatTransport`。
#[async_trait]
pub trait Backoff: Send {
    /// **参数**：`delay` 是这次失败之后要等待的时长。`cancel` 在等待中被触发时立刻结束。
    async fn wait(&mut self, delay: Duration, cancel: &CancellationToken) -> Result<(), ChatError>;
}

/// 记下等待间隔，不睡眠。
///
/// **作用**：断言 429 / 5xx 的退避序列。
/// **边界**：不访问时钟，也不连接网络。令牌已取消时不记录间隔，直接返回 `Cancelled`。
#[derive(Debug, Default)]
pub struct RecordingBackoff {
    pub waits: Vec<Duration>,
}

#[async_trait]
impl Backoff for RecordingBackoff {
    /// **作用**：把这次退避间隔记下来，让测试断言 200ms、400ms、800ms。
    /// **参数**：`delay` 是 `execute` 要求等待的时长。`cancel` 已触发时不写入 `waits`。
    /// **边界**：不睡眠，也不连接网络。令牌已触发时返回 `Cancelled`。
    async fn wait(&mut self, delay: Duration, cancel: &CancellationToken) -> Result<(), ChatError> {
        if cancel.is_cancelled() {
            return Err(ChatError::Cancelled);
        }
        self.waits.push(delay);
        Ok(())
    }
}

/// 真正睡眠的退避等待。
///
/// **作用**：给 `DeepSeekClient` 配套，让 429 / 5xx 的重试之间真有 200ms、400ms、800ms 间隔。
/// **边界**：用 tokio 计时器，调用方必须在 tokio 运行时里。等待期间令牌触发就立刻返回 `Cancelled`。不加抖动。
#[derive(Debug, Default)]
pub struct SleepBackoff;

#[async_trait]
impl Backoff for SleepBackoff {
    /// **作用**：睡满 `delay`，或在令牌触发时提前结束。
    /// **参数**：`delay` 是 `execute` 要求等待的时长。`cancel` 触发时不再等待。
    /// **边界**：令牌已触发时不睡眠，直接返回 `Cancelled`。睡满返回 `Ok`。
    async fn wait(&mut self, delay: Duration, cancel: &CancellationToken) -> Result<(), ChatError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ChatError::Cancelled),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

/// 按重试规则调用传输层，直到成功或不可恢复。
///
/// **作用**：把 Chat Completions 请求交给 `ChatTransport`。
/// **参数**：`transport` 执行一次已序列化的请求。`request` 是要发送的消息和工具。`backoff` 只负责按给定间隔等待，测试里可以不睡眠。`cancel` 在发送、读块和退避时都会检查。
/// **边界**：最多 1 次初始调用加 3 次重试。400、流中断、取消、非法工具参数、畸形工具调用立刻返回。取消返回 `Cancelled`，流中断返回 `StreamInterrupted`。
/// 返回的 future 是 `Send`，由 `ChatTransport` / `Backoff` 的 `Send` 约束保证。
/// `Transport`（DNS、连接在拿到状态码前失败）也立刻返回。P0 不把这类抖动当成可退避错误，以后可以再加重试。
/// 流中断与取消都不附带已拼接的片段，调用方不得把它写入下一轮历史。不读取 `DEEPSEEK_API_KEY`。
pub async fn execute(
    transport: &mut dyn ChatTransport,
    request: &ChatRequest,
    backoff: &mut dyn Backoff,
    cancel: &CancellationToken,
) -> Result<ChatCompletion, ChatError> {
    let request_json = serde_json::to_value(request).map_err(|_| ChatError::NotEncodable)?;
    let mut retries = 0;
    loop {
        match transport.round(&request_json, cancel).await? {
            Round::Completed(completion) => return Ok(completion),
            Round::Interrupted => return Err(ChatError::StreamInterrupted),
            Round::Cancelled => return Err(ChatError::Cancelled),
            Round::InvalidToolArguments => return Err(ChatError::InvalidToolArguments),
            Round::MalformedToolCall => return Err(ChatError::MalformedToolCall),
            Round::Status { status, summary } if is_retryable(status) => {
                if retries == RETRY_DELAYS.len() {
                    return Err(ChatError::Unavailable { status, summary });
                }
                backoff.wait(RETRY_DELAYS[retries], cancel).await?;
                retries += 1;
            }
            Round::Status { status, summary } => {
                return Err(ChatError::Rejected { status, summary })
            }
        }
    }
}

/// 判断 HTTP 状态是否值得退避重试。
///
/// **参数**：`status` 是传输层返回的状态码。只有 429 和 5xx 返回真。
/// **边界**：400 和其他 4xx 返回假，调用方应立刻拒绝。
fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}
