use std::time::Duration;

use serde_json::Value;

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
/// **边界**：400 不重试。429 与 5xx 最多重试 3 次。流中断和非法工具参数不重试。
#[derive(Debug, PartialEq, Eq)]
pub enum Round {
    Completed(ChatCompletion),
    Status(u16),
    Interrupted,
    InvalidToolArguments,
}

/// 发送一次已序列化的请求。
///
/// **作用**：Fake 和以后的 `reqwest` 客户端都走这个入口。
/// **边界**：实现不读密钥。请求 JSON 只用于协议判断，不写入日志。
pub trait ChatTransport {
    /// **参数**：`request_json` 是已经序列化的请求。实现用它判断协议，不写入日志。
    fn round(&mut self, request_json: &Value) -> Result<Round, ChatError>;
}

/// 重试之间的等待。
///
/// **作用**：测试可以只记录间隔，不真正睡觉。
/// **边界**：本切片没有真实 HTTP，因此不提供会阻塞线程的实现。
pub trait Backoff {
    /// **参数**：`delay` 是这次失败之后要等待的时长。实现可以只记录它。
    fn wait(&mut self, delay: Duration);
}

/// 记下等待间隔，不睡眠。
///
/// **作用**：断言 429 / 5xx 的退避序列。
/// **边界**：不访问时钟，也不连接网络。
#[derive(Debug, Default)]
pub struct RecordingBackoff {
    pub waits: Vec<Duration>,
}

impl Backoff for RecordingBackoff {
    fn wait(&mut self, delay: Duration) {
        self.waits.push(delay);
    }
}

/// 按重试规则调用传输层，直到成功或不可恢复。
///
/// **作用**：把 Chat Completions 请求交给 `ChatTransport`。
/// **参数**：`transport` 执行一次已序列化的请求。`request` 是要发送的消息和工具。`backoff` 只负责按给定间隔等待，测试里可以不睡眠。
/// **边界**：最多 1 次初始调用加 3 次重试。400、流中断、非法工具参数立刻返回。
/// 流中断不附带已拼接的片段，调用方不得把它写入下一轮历史。不读取 `DEEPSEEK_API_KEY`。
pub fn execute(
    transport: &mut dyn ChatTransport,
    request: &ChatRequest,
    backoff: &mut dyn Backoff,
) -> Result<ChatCompletion, ChatError> {
    let request_json = serde_json::to_value(request).map_err(|_| ChatError::NotEncodable)?;
    let mut retries = 0;
    loop {
        match transport.round(&request_json)? {
            Round::Completed(completion) => return Ok(completion),
            Round::Interrupted => return Err(ChatError::StreamInterrupted),
            Round::InvalidToolArguments => return Err(ChatError::InvalidToolArguments),
            Round::Status(status) if is_retryable(status) => {
                if retries == RETRY_DELAYS.len() {
                    return Err(ChatError::Unavailable { status });
                }
                backoff.wait(RETRY_DELAYS[retries]);
                retries += 1;
            }
            Round::Status(status) => return Err(ChatError::Rejected { status }),
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
