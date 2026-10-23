use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatError, API_KEY_ENV};
use crate::retry::{ChatTransport, Round};
use crate::sse::parse_sse;

/// 官方 Chat Completions 的默认根地址。
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// 建立连接的上限，含 TLS。文档没有规定秒数。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 两次成功读到数据之间允许的空闲时间。有字节到达就会重新计时，不限制整次响应的总时长。
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次 SSE 响应体允许累积的最大字节数。超过按流中断处理，防止无界占用内存。
const MAX_SSE_BODY_BYTES: usize = 16 * 1024 * 1024;

/// 非 2xx 响应体允许读取的最大字节数。只用来提取错误摘要，超过则摘要为空。
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// 调用 DeepSeek Chat Completions 的异步客户端。
///
/// **作用**：实现 `ChatTransport`，把已序列化的请求用 SSE 发到 `/chat/completions`。
/// **边界**：`Debug` 只保留密钥长度。不记录请求正文和思维链。
/// 不设整次请求的总超时。连接超过 10 秒，或两个数据块之间空闲超过 60 秒，才是流中断，并且不重试。
/// 这一刀不解析 `delta.tool_calls`。
/// `round` 是异步的：按块读取 SSE，块与块之间检查取消令牌。取消、超时或 `[DONE]` 前断开都不返回半截文本。
pub struct DeepSeekClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl std::fmt::Debug for DeepSeekClient {
    /// **作用**：日志里能看出连的是哪台服务。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：只写 `base_url` 和密钥长度。不写 Bearer 令牌、请求正文或思维链。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeepSeekClient")
            .field("base_url", &self.base_url)
            .field("api_key_len", &self.api_key.len())
            .finish()
    }
}

impl DeepSeekClient {
    /// 用显式密钥构造客户端。
    ///
    /// **作用**：测试和 `from_env` 共用这一条路径。
    /// **参数**：`api_key` 是 Bearer 令牌。`base_url` 是 API 根，不含 `/chat/completions`。
    /// **边界**：先去掉首尾空白。去掉后为空才是 `MissingCredential`。中间仍有空白则不发送，同样返回 `MissingCredential`。
    /// 只设置连接超时和读空闲超时，不设置整次请求的总时长。不创建私有运行时；调用方提供 tokio 运行时。
    pub fn from_key(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, ChatError> {
        let api_key = api_key.into().trim().to_owned();
        if api_key.is_empty() || api_key.chars().any(char::is_whitespace) {
            return Err(ChatError::MissingCredential);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_IDLE_TIMEOUT)
            .build()
            .map_err(|_| ChatError::Transport)?;
        Ok(Self {
            http,
            api_key,
            base_url: base_url.into(),
        })
    }

    /// 从 `DEEPSEEK_API_KEY` 构造客户端。
    ///
    /// **作用**：真实调用的入口。
    /// **边界**：环境变量缺失或去掉首尾空白后为空时返回 `MissingCredential`。错误文本不包含密钥。
    pub fn from_env() -> Result<Self, ChatError> {
        let key = std::env::var(API_KEY_ENV).unwrap_or_default();
        Self::from_key(key, DEFAULT_BASE_URL)
    }
}

#[async_trait]
impl ChatTransport for DeepSeekClient {
    /// **作用**：把这次请求 POST 到 `/chat/completions`，按块读 SSE，再交给 `parse_sse`。
    /// **参数**：`request_json` 是已经序列化的请求。这里会复制一份并写入 `stream: true` 和 `stream_options.include_usage: true`，不改调用方的值。`cancel` 在发送和每个数据块之间检查。
    /// **边界**：取消返回 `Cancelled`。连接超时或读空闲超时返回 `Interrupted`。两者都不重试，不返回已拼片段。DNS 或连接在拿到状态码前失败返回 `Transport`，P0 也不退避。不记录请求正文和密钥。
    async fn round(
        &mut self,
        request_json: &Value,
        cancel: &CancellationToken,
    ) -> Result<Round, ChatError> {
        if cancel.is_cancelled() {
            return Ok(Round::Cancelled);
        }
        let body = stream_request_body(request_json)?;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let send = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&Value::Object(body))
            .send();
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(Round::Cancelled),
            response = send => response,
        };
        match response {
            Err(error) if error.is_timeout() => Ok(Round::Interrupted),
            Err(_) => Err(ChatError::Transport),
            Ok(response) => read_response(response, cancel).await,
        }
    }
}

/// 给已序列化的请求补上流式和用量开关。
///
/// **作用**：`round` 发出去的正文一定带 `stream: true` 和 `stream_options.include_usage: true`。
/// **参数**：`request_json` 必须是对象。这个函数不修改调用方持有的值。
/// **边界**：`stream_options` 不能单独出现，否则官方接口返回 400。末块用量不依赖「无论是否设置都会返回」那一句。
pub fn stream_request_body(
    request_json: &Value,
) -> Result<serde_json::Map<String, Value>, ChatError> {
    let mut body = request_json
        .as_object()
        .cloned()
        .ok_or(ChatError::NotEncodable)?;
    body.insert("stream".to_owned(), json!(true));
    body.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );
    Ok(body)
}

/// 非 2xx 响应里可安全留下的说明。
const ERROR_MESSAGE_LIMIT: usize = 160;

/// 从错误响应里抽出一段短说明。
///
/// **作用**：400 排查时能看到 Provider 的 `message`，同时不把整段正文留在错误里。
/// **参数**：`bytes` 是响应体。只接受 JSON。
/// **边界**：取 `error.message`，否则取顶层 `message`。去掉控制字符后最多 160 个字符。不是这个形状时返回空串，不抄原始正文。
fn provider_error_summary(bytes: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return String::new();
    };
    let Some(message) = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
    else {
        return String::new();
    };
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(ERROR_MESSAGE_LIMIT)
        .collect()
}

/// 读取一次 HTTP 响应。
///
/// **作用**：把状态码和 SSE 正文变成 `Round`。
/// **参数**：`response` 是已经返回状态码的响应。`cancel` 在读错误摘要或 SSE 块时检查。非 2xx 只保留状态码，以及 JSON 里 `error.message` 或 `message` 的最多 160 个字符。
/// **边界**：取消返回 `Cancelled`。2xx 但流不完整、读空闲过久、正文超过上限或不是 UTF-8，返回 `Interrupted`。都不把半截文本交给调用方。
/// 按块累积后再解析，中途取消丢弃缓冲。非 2xx 的正文读失败或超过上限时摘要为空，状态码照常交回。
async fn read_response(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Round, ChatError> {
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let summary = match read_capped(response, cancel, MAX_ERROR_BODY_BYTES).await {
            BodyRead::Complete(bytes) => provider_error_summary(&bytes),
            BodyRead::Cancelled => return Ok(Round::Cancelled),
            BodyRead::Broken => String::new(),
        };
        return Ok(Round::Status { status, summary });
    }
    let bytes = match read_capped(response, cancel, MAX_SSE_BODY_BYTES).await {
        BodyRead::Complete(bytes) => bytes,
        BodyRead::Cancelled => return Ok(Round::Cancelled),
        BodyRead::Broken => return Ok(Round::Interrupted),
    };
    let Ok(body) = String::from_utf8(bytes) else {
        return Ok(Round::Interrupted);
    };
    match parse_sse(&body) {
        Ok(completion) => Ok(Round::Completed(completion)),
        Err(ChatError::StreamInterrupted) => Ok(Round::Interrupted),
        Err(error) => Err(error),
    }
}

/// 一次按块读取的结局。
///
/// **作用**：让 `read_response` 区分取消和读失败，二者对应不同的 `Round`。
/// **边界**：`Broken` 包含读空闲超时、网络读错误和超过字节上限。不携带已读到的片段。
enum BodyRead {
    Complete(Vec<u8>),
    Cancelled,
    Broken,
}

/// 按块读完响应体，并设字节上限。
///
/// **作用**：在每个数据块之间检查取消令牌，凑齐后交回完整字节。
/// **参数**：`response` 是已有状态码的响应。`cancel` 触发时立刻停下。`limit` 是允许累积的最大字节数。
/// **边界**：取消返回 `Cancelled`。超时、读失败或累积超过 `limit` 返回 `Broken`。这两种都丢弃缓冲，不交出半截内容。
async fn read_capped(
    response: reqwest::Response,
    cancel: &CancellationToken,
    limit: usize,
) -> BodyRead {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return BodyRead::Cancelled,
            next = stream.next() => next,
        };
        match next {
            None => return BodyRead::Complete(body),
            Some(Ok(chunk)) if body.len() + chunk.len() <= limit => body.extend_from_slice(&chunk),
            Some(_) => return BodyRead::Broken,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::provider_error_summary;

    /// JSON 错误只留下 `error.message`。
    ///
    /// **边界**：控制字符去掉。不是该字段的正文，包括像 prompt 的纯文本，不进入摘要。
    #[test]
    fn error_summary_keeps_only_the_message_field() {
        let summary = provider_error_summary(
            br#"{"error":{"message":"bad model\nname","type":"invalid_request_error"}}"#,
        );
        assert_eq!(summary, "bad modelname");
        assert!(provider_error_summary(b"user prompt stays out").is_empty());
        let long = format!("{{\"message\":\"{}\"}}", "x".repeat(200));
        assert_eq!(provider_error_summary(long.as_bytes()).chars().count(), 160);
    }
}
