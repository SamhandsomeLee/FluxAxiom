use fluxaxiom_contract::{ModelUsage, TokenCount};
use serde::Serialize;
use serde_json::Value;

/// 真实调用读取的环境变量名。
///
/// **作用**：以后的 HTTP 客户端只从这里取密钥。
/// **边界**：本模块的 `execute` 和 Fake 都不读环境变量，也不把这个名字写进日志。
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// P0 默认模型别名。运行时不猜测“最新”。
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// `ChatRequest::new` 写入的输出上限。未校准的暂定值，见执行方案 §4.7.5。
pub const DEFAULT_MAX_TOKENS: u32 = 32_768;

/// DeepSeek 文档给出的 `max_tokens` 上限。超出时 `with_max_tokens` 返回 `NotEncodable`。
pub const MAX_TOKENS_LIMIT: u32 = 393_216;

/// 思考强度。序列化为 `low` / `high` / `max`。
///
/// **作用**：放进 Chat Completions 请求。
/// **边界**：默认 `high`。P0 不发送 `temperature` 或 penalty。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    #[default]
    High,
    Max,
}

/// 一条对话消息。
///
/// **作用**：组成发给 DeepSeek 的 `messages`。
/// **边界**：assistant 的 `reasoning_content` 是 `String`，序列化时这个键一定在。空字符串会写成 `""`，不会省略，也不会写成 `null`。
/// 类型表达不了“没有这个键”。那种 JSON 只在 Fake 的原始报文测试里出现。
/// `Debug` 只打印长度，不打印正文。
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        reasoning_content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

impl std::fmt::Debug for Message {
    /// **作用**：让日志能看出消息角色和文本有多长。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：不写入 `content` 或 `reasoning_content` 的原文。工具调用只写个数。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System { content } => formatter
                .debug_struct("System")
                .field("content_len", &content.len())
                .finish(),
            Self::User { content } => formatter
                .debug_struct("User")
                .field("content_len", &content.len())
                .finish(),
            Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => formatter
                .debug_struct("Assistant")
                .field("content_len", &content.len())
                .field("reasoning_len", &reasoning_content.len())
                .field("tool_calls", &tool_calls.len())
                .finish(),
            Self::Tool {
                content,
                tool_call_id,
            } => formatter
                .debug_struct("Tool")
                .field("content_len", &content.len())
                .field("tool_call_id", tool_call_id)
                .finish(),
        }
    }
}

/// 模型发起的一次工具调用。
///
/// **作用**：在内存里保存调用 id、工具名和参数原文。
/// **边界**：`arguments` 必须是 JSON 文本。非法 JSON 视为这一次调用失败，不重试。
/// 序列化成 OpenAI 兼容的嵌套形状：`type` 为 `function`，`name` 和 `arguments` 放在 `function` 里。
/// 形状来自思考模式文档样例里的 SDK `tool_calls`，这次没有单独打工具接口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Serialize for ToolCall {
    /// **作用**：把内存里的平铺字段收成线上的嵌套对象。
    /// **参数**：`serializer` 接收 `{"id","type":"function","function":{"name","arguments"}}`。
    /// **边界**：不输出顶层的 `name` 或 `arguments`。形状来自思考模式文档样例，不是这次对真实 API 的调用。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            id: &'a str,
            #[serde(rename = "type")]
            kind: &'static str,
            function: Function<'a>,
        }
        #[derive(Serialize)]
        struct Function<'a> {
            name: &'a str,
            arguments: &'a str,
        }
        Wire {
            id: &self.id,
            kind: "function",
            function: Function {
                name: &self.name,
                arguments: &self.arguments,
            },
        }
        .serialize(serializer)
    }
}

/// 请求里声明的一个工具。
///
/// **作用**：在内存里保存名字、说明和参数 schema。
/// **边界**：执行工具是 A2.2 的事。序列化成 `{"type":"function","function":{name,description,parameters}}`。
/// 这是思考模式文档样例里的声明形状，不是这次对真实 API 的调用结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl Serialize for ToolSpec {
    /// **作用**：把工具声明收成线上的 `function` 对象。
    /// **参数**：`serializer` 接收 `{"type":"function","function":{"name","description","parameters"}}`。
    /// **边界**：`description` 即使为空也保留该键。不在顶层写 `name` 或 `parameters`。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            function: Function<'a>,
        }
        #[derive(Serialize)]
        struct Function<'a> {
            name: &'a str,
            description: &'a str,
            parameters: &'a Value,
        }
        Wire {
            kind: "function",
            function: Function {
                name: &self.name,
                description: &self.description,
                parameters: &self.parameters,
            },
        }
        .serialize(serializer)
    }
}

/// 一次 Chat Completions 请求。
///
/// **作用**：序列化后交给传输层。
/// **边界**：不包含密钥。不把 `reasoning_content` 写入日志或 event。
/// `tools` 为 `None` 时不出现这个键。空列表也不会写成 `[]`，因为官方文档没有定义空数组算不算“携带 tools”。
/// `max_tokens` 为 `None` 时不出现这个键，才会落到服务端默认档位；`new` 默认写入 [`DEFAULT_MAX_TOKENS`]，避免调用方忘了设上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl ChatRequest {
    /// 用默认思考强度和输出上限组装请求，不带工具。
    ///
    /// **作用**：调用方只提供模型和消息。
    /// **参数**：`model` 是请求的模型别名。`messages` 按顺序成为对话历史。
    /// **边界**：不写入 `tools` 键。`max_tokens` 为 [`DEFAULT_MAX_TOKENS`]。Engine A 需要工具时再调用 `with_tools`。要用服务端默认上限时调用 `without_max_tokens`。
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            reasoning_effort: ReasoningEffort::default(),
            messages,
            tools: None,
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        }
    }

    /// 给这次请求带上工具。
    ///
    /// **作用**：让序列化结果出现 `tools` 键。带上之后，历史 assistant 的 `reasoning_content` 必须回传。
    /// **参数**：`tools` 是本次声明的工具。空列表会被丢掉，避免发出未经证实的 `[]`。
    /// **边界**：这是 Engine 是否带工具的选择，不是所有请求的类型不变量。
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = if tools.is_empty() { None } else { Some(tools) };
        self
    }

    /// 覆盖输出 token 上限。
    ///
    /// **作用**：让序列化结果带上调用方选定的 `max_tokens`。
    /// **参数**：`max_tokens` 必须在 `1..=MAX_TOKENS_LIMIT`（393216）。
    /// **边界**：超出范围返回 `NotEncodable`，请求不会发出。这是本地校验，不是 HTTP 状态。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Result<Self, ChatError> {
        if !(1..=MAX_TOKENS_LIMIT).contains(&max_tokens) {
            return Err(ChatError::NotEncodable);
        }
        self.max_tokens = Some(max_tokens);
        Ok(self)
    }

    /// 去掉请求里的 `max_tokens` 键。
    ///
    /// **作用**：显式使用服务端默认档位。官方文档记载的默认值按模式区分：非思考模式 8K，思考模式 64K，`reasoning_effort` 为 `max` 时 128K；不是按型号区分。
    /// 思维链是否计入 `max_tokens`、触顶时 `reasoning_content` 与半截工具调用怎么处理，文档没写，见 ADR-020 的 API 观察。
    /// **边界**：A2.2 要求上限生效时不要默认走这条路径；`new` 已经写入暂定值。
    #[must_use]
    pub fn without_max_tokens(mut self) -> Self {
        self.max_tokens = None;
        self
    }
}

/// 模型结束原因。`length` 表示输出被截断，但仍是一次成功响应。
///
/// **边界**：`Length` 的响应里 `tool_calls` 一律为空：被截断的调用不能执行，也不能写进历史。`content` 可能是半截文本。Run 的终态由 Loop 决定，见执行方案 §4.7.5。
/// 未知原因放在 `Other` 里，原文要留给 `EngineObservation` 的非空 `finish_reason`。`Debug` 不打印这段原文。
#[derive(Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    /// 文档样例之外的结束原因。字符串是 Provider 原文。
    Other(String),
}

impl std::fmt::Debug for FinishReason {
    /// **作用**：日志里区分已知结束原因。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：`Other` 只打印变体名。原文留在值里，不进 `Debug`。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stop => formatter.write_str("Stop"),
            Self::Length => formatter.write_str("Length"),
            Self::ToolCalls => formatter.write_str("ToolCalls"),
            Self::Other(_) => formatter.write_str("Other"),
        }
    }
}

/// 一次已经成功结束的模型响应。
///
/// **作用**：把流式 delta 收成 Host 以后可以放入 blob 的完整文本。
/// **边界**：只有成功结束才产生这个值。流中断或 400 不返回半截 `reasoning_content`，调用方也不能把它写入下一轮历史。
/// 本 crate 不写 blob，也不写 durable event。`Debug` 只打印长度，不打印正文。
/// `reasoning` 与 `content` 用观测值：字段没出现是 `Absent`，不用空串填上。
/// `finish_reason` 是 `Length` 时 `tool_calls` 为空，其余情况下每个调用的 `id`、`name` 非空、`arguments` 是合法 JSON。
#[derive(Clone, PartialEq, Eq)]
pub struct ChatCompletion {
    pub reasoning: ObservedText,
    pub content: ObservedText,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: ModelUsage,
}

impl std::fmt::Debug for ChatCompletion {
    /// **作用**：日志里能看出结束原因、用量和文本长度。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：不写入思维链或正文。`Round` 包含这个值，因此也不会带出原文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatCompletion")
            .field("reasoning_len", &observed_len(&self.reasoning))
            .field("content_len", &observed_len(&self.content))
            .field("tool_calls", &self.tool_calls.len())
            .field("finish_reason", &self.finish_reason)
            .field("usage", &self.usage)
            .finish()
    }
}

/// 客户端失败。
///
/// **作用**：区分不能重试的拒绝、重试耗尽、流中断和非法工具参数。
/// **边界**：不携带请求正文或密钥。400 用 `Rejected`。429 / 5xx 重试耗尽后用 `Unavailable`。二者的 `summary` 只可能是截断后的 `error.message`。
/// 本地序列化失败用 `NotEncodable`，缺密钥用 `MissingCredential`，连接失败用 `Transport`。这些都不是 HTTP 状态码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatError {
    Rejected {
        status: u16,
        /// 最多 160 个字符，只来自 JSON 的 `error.message` 或 `message`。没有则空。
        summary: String,
    },
    Unavailable {
        status: u16,
        /// 最多 160 个字符，只来自 JSON 的 `error.message` 或 `message`。没有则空。
        summary: String,
    },
    /// 流在完成前断开。没有可回传的 `reasoning_content`。
    StreamInterrupted,
    /// 调用方触发了取消令牌。和 `StreamInterrupted` 分开，Host 据此走 `Cancelling → Cancelled`。同样没有可回传的片段。
    Cancelled,
    /// 工具调用的 `arguments` 拼完后不是合法 JSON（含空串）。不重试。
    InvalidToolArguments,
    /// 流完整收尾，但工具调用形状违约：缺 `index`、缺 `id`、名字为空、同 index 冲突、同响应 id 重复、或 `type` 不是 `function`。
    /// 不能产出半截 `ToolCall` 交给 Loop，否则空 `tool_call_id` 会让下一轮 400。
    MalformedToolCall,
    /// 请求在离开本进程之前无法序列化，或本地参数越界。这不是 Provider 返回的 HTTP 状态。
    NotEncodable,
    /// 没有 `DEEPSEEK_API_KEY`，因此没有发出请求。
    MissingCredential,
    /// 还没拿到 HTTP 状态就失败了，例如无法连接。不是超时。
    Transport,
}

impl std::fmt::Display for ChatError {
    /// **作用**：给调用方一句可读的失败说明。
    /// **参数**：`formatter` 接收显示文本。
    /// **边界**：状态码之外最多再写 160 个字符的 `error.message`。不写原始响应体、思维链或密钥。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { status, summary } => {
                write_status(formatter, "rejected", *status, summary)
            }
            Self::Unavailable { status, summary } => {
                write_status(formatter, "unavailable", *status, summary)
            }
            Self::StreamInterrupted => formatter.write_str("chat stream interrupted"),
            Self::Cancelled => formatter.write_str("chat cancelled"),
            Self::InvalidToolArguments => formatter.write_str("tool call arguments are not json"),
            Self::MalformedToolCall => formatter.write_str("tool call stream was malformed"),
            Self::NotEncodable => formatter.write_str("chat request could not be encoded"),
            Self::MissingCredential => formatter.write_str("deepseek api key is missing"),
            Self::Transport => formatter.write_str("chat transport failed before a status"),
        }
    }
}

impl std::error::Error for ChatError {}

/// 写出带可选摘要的 HTTP 失败。
///
/// **作用**：`Display` 在有摘要时带上截断后的 Provider 说明。
/// **参数**：`kind` 是 `rejected` 或 `unavailable`。`summary` 已经截断并去掉控制字符。
/// **边界**：摘要为空时只写状态码。不写原始响应体。
fn write_status(
    formatter: &mut std::fmt::Formatter<'_>,
    kind: &str,
    status: u16,
    summary: &str,
) -> std::fmt::Result {
    if summary.is_empty() {
        write!(formatter, "chat request {kind}: {status}")
    } else {
        write!(formatter, "chat request {kind}: {status}: {summary}")
    }
}

/// 一段文本字段实际出现过什么。
///
/// **作用**：把“没这个键”“空字符串”“有文本”分开，避免反序列化时编造空串。思维链和正文共用这一形状。
/// **边界**：`null` 和其他类型都视为没有这个键。官方文档没有定义 `null`。`Debug` 只打印变体和长度，不打印文本。
#[derive(Clone, PartialEq, Eq)]
pub enum ObservedText {
    Absent,
    Empty,
    Text(String),
}

impl std::fmt::Debug for ObservedText {
    /// **作用**：日志里能看出字段有没有文本。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：`Text` 只打印字符数。思维链和正文都不进 `Debug`。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => formatter.write_str("Absent"),
            Self::Empty => formatter.write_str("Empty"),
            Self::Text(text) => formatter.debug_tuple("Text").field(&text.len()).finish(),
        }
    }
}

/// 下一轮要不要回传思维链。
///
/// **作用**：带 `tools` 时给出要写入 assistant 消息的字符串。不带 `tools` 时不回传。
/// **边界**：`filled_by_host` 为真表示空串是我们补的，不是 Provider 原文。blob 必须把这件事和原始响应分开保存。`Debug` 只打印长度。
#[derive(Clone, PartialEq, Eq)]
pub struct ReplayReasoning {
    pub reasoning_content: Option<String>,
    pub filled_by_host: bool,
}

impl std::fmt::Debug for ReplayReasoning {
    /// **作用**：日志里能看出回传了多长的思维链，以及是不是 Host 补的空串。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：不打印 `reasoning_content` 原文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let length = self.reasoning_content.as_ref().map_or(0, String::len);
        formatter
            .debug_struct("ReplayReasoning")
            .field("reasoning_len", &length)
            .field("filled_by_host", &self.filled_by_host)
            .finish()
    }
}

/// 从一条原始 assistant JSON 读取思维链。
///
/// **作用**：保留 Provider 有没有给出这个字段。
/// **参数**：`message` 是响应或历史里的一条消息对象。
/// **边界**：不用 `#[serde(default)]`。缺键和 `null` 都是 `Absent`，不会变成 `""`。
#[must_use]
pub fn observe_reasoning(message: &Value) -> ObservedText {
    match message.get("reasoning_content") {
        Some(Value::String(text)) if text.is_empty() => ObservedText::Empty,
        Some(Value::String(text)) => ObservedText::Text(text.clone()),
        None | Some(_) => ObservedText::Absent,
    }
}

/// 决定下一轮 assistant 消息如何携带思维链。
///
/// **作用**：带工具的请求必须回传该字段；不带工具的请求不把它放进历史。
/// **参数**：`observed` 是 Provider 原文里看到的值。`request_has_tools` 表示这一轮请求是否携带 `tools`。
/// **边界**：带工具但原文缺失或为 `null` 时，回传 `""` 并标记 `filled_by_host`。空字符串原文不标记为补写。
#[must_use]
pub fn replay_reasoning(observed: ObservedText, request_has_tools: bool) -> ReplayReasoning {
    if !request_has_tools {
        return ReplayReasoning {
            reasoning_content: None,
            filled_by_host: false,
        };
    }
    match observed {
        ObservedText::Text(text) => ReplayReasoning {
            reasoning_content: Some(text),
            filled_by_host: false,
        },
        ObservedText::Empty => ReplayReasoning {
            reasoning_content: Some(String::new()),
            filled_by_host: false,
        },
        ObservedText::Absent => ReplayReasoning {
            reasoning_content: Some(String::new()),
            filled_by_host: true,
        },
    }
}

/// 从 usage JSON 取出计数。
///
/// **作用**：保留缓存命中字段。
/// **参数**：`value` 是响应里的 usage 对象。缺字段和 `"unknown"` 都按未知处理。
/// **边界**：数字 0 是 `Known(0)`，不用 0 表示未知。
#[must_use]
pub fn usage_from_json(value: &Value) -> ModelUsage {
    ModelUsage {
        prompt_tokens: token_count(value, "prompt_tokens"),
        completion_tokens: token_count(value, "completion_tokens"),
        prompt_cache_hit_tokens: token_count(value, "prompt_cache_hit_tokens"),
        prompt_cache_miss_tokens: token_count(value, "prompt_cache_miss_tokens"),
    }
}

/// 把已经拼好的文本收成观测值。
///
/// **参数**：`text` 是该字段出现过之后的拼接结果。
/// **边界**：空文本是 `Empty`。调用方在字段从未出现时不要调用它。
#[must_use]
pub fn observe_reasoning_text(text: &str) -> ObservedText {
    if text.is_empty() {
        ObservedText::Empty
    } else {
        ObservedText::Text(text.to_owned())
    }
}

/// `Debug` 只报告文本长度。
///
/// **参数**：`value` 是思维链或正文的观测值。
/// **边界**：`Empty` 和 `Absent` 都是 0。不返回原文。
fn observed_len(value: &ObservedText) -> usize {
    match value {
        ObservedText::Text(text) => text.len(),
        ObservedText::Empty | ObservedText::Absent => 0,
    }
}

/// 读取 usage 对象里的一个计数字段。
///
/// **参数**：`value` 是 usage JSON。`key` 是字段名，例如 `prompt_cache_hit_tokens`。
/// **边界**：字段缺失、值为 `"unknown"`、负数、小数或其他类型时返回 `Unknown`。不用 0 冒充。
fn token_count(value: &Value, key: &str) -> TokenCount {
    match value.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .map_or(TokenCount::Unknown, TokenCount::Known),
        Some(Value::String(text)) if text == "unknown" => TokenCount::Unknown,
        None => TokenCount::Unknown,
        Some(_) => TokenCount::Unknown,
    }
}
