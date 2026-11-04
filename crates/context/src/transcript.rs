//! 调用方交来的对话事实，以及恒等策略交回的视图。
//!
//! **作用**：`prepare` 读 [`Transcript`]，`replay` 写出 [`View`]。
//! **边界**：工具正文由调用方填入。本模块不打开 `result_ref`。`Debug` 只打印长度。

use std::fmt;

use fluxaxiom_contract::ModelUsage;

use crate::text::{count_text, Count};

/// 一次模型调用之前已经提交的事实。
///
/// **作用**：描述将要发给模型的历史。最后一轮的工具结果是下一次请求的新增内容。
/// **边界**：`user` 是首条用户消息。一轮之后另起的用户文本放在 [`Round::trailing_user`]。
#[derive(Clone, PartialEq, Eq)]
pub struct Transcript {
    /// 系统提示正文。空串允许。`Debug` 只打印字符数，不进入 context 记录。
    pub system: String,
    /// 首条用户消息正文。空串允许。这一轮之后新出现的用户文本放在 [`Round::trailing_user`]。`Debug` 只打印字符数。
    pub user: String,
    /// 已完成的模型轮，从旧到新。空 `Vec` 表示还没有模型调用。下标 0 对应 `trace` 的第 1 轮。
    pub rounds: Vec<Round>,
}

/// 同一次 Attempt 里的一次模型调用，及其后的工具结果。
///
/// **作用**：对应 `trace` 从 1 起的模型轮。`usage` 是这一轮响应里的用量。
/// **边界**：`usage` 的缓存字段不参与锚点判断。
#[derive(Clone, PartialEq, Eq)]
pub struct Round {
    /// 这一轮模型返回的 assistant 消息。
    pub assistant: AssistantTurn,
    /// 这一轮响应之后产生的工具结果，顺序与响应里的调用相同。没有工具时为空 `Vec`。
    pub tools: Vec<ToolResult>,
    /// 这一轮响应的用量。`prompt_tokens` 或 `completion_tokens` 为 `Unknown` 时，这一轮不能当锚点。缓存字段不参与判断。
    pub usage: ModelUsage,
    /// 工具结果之后、下一次模型调用之前新出现的用户文本。没有时为空串。`Debug` 只打印字符数。
    pub trailing_user: String,
}

/// 一轮 assistant 消息。
///
/// **作用**：保存正文、思维链和工具调用。锚点成立时这些已含在 `completion_tokens` 里。
/// **边界**：`Debug` 不打印这些字符串。
#[derive(Clone, PartialEq, Eq)]
pub struct AssistantTurn {
    /// 给用户看的正文。空串允许。`Debug` 不打印原文。锚点成立时，这段已含在该轮的 `completion_tokens` 里。
    pub content: String,
    /// 思维链。空串允许。`Debug` 不打印原文，也不进入 context 记录。
    pub reasoning: String,
    /// 这一轮的工具调用，顺序与响应一致。没有时为空 `Vec`。
    pub tool_calls: Vec<ToolCall>,
}

/// assistant 消息上的一次工具调用。
///
/// **作用**：全量估算时计入 `id`、`name`、`arguments` 里的汉字、英文字母和 ASCII 数字。
/// **边界**：锚点估算不再数它们。
#[derive(Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 调用 ID，应与对应 [`ToolResult::call_id`] 相同。空串在这里不拒绝。全量估算会计入其中的英文字母和 ASCII 数字。`Debug` 会打印。
    pub id: String,
    /// 工具名。空串在这里不拒绝。全量估算计入汉字、英文字母和 ASCII 数字。`Debug` 会打印。
    pub name: String,
    /// 参数 JSON 文本。空串允许。`Debug` 只打印字符数。全量估算计入汉字、英文字母和 ASCII 数字；锚点估算不数。
    pub arguments: String,
}

/// 模型调用之后产生的一条工具结果。
///
/// **作用**：`body` 是调用方交来的正文，供估算和以后的视图使用。
/// **参数**：`byte_count` 是调用方声明的字节数。估算读 `body`，不拿它和 `byte_count` 对账。
/// **边界**：`result_ref` 不进入 token 计数，也不进入 context 记录以外的地方；记录本身不含正文。
#[derive(Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// 对应工具调用的 ID。空串在这里不拒绝。`Debug` 会打印。估算不数。
    pub call_id: String,
    /// 工具名。估算不数，避免和 assistant 上的同一次调用重复。`Debug` 会打印。
    pub name: String,
    /// 参数文本。估算不数。`Debug` 不打印原文。
    pub arguments: String,
    /// 正文 blob 的引用，通常是 `sha256:` 加十六进制。估算不数，本模块也不打开它。`Debug` 会打印。
    pub result_ref: String,
    /// 调用方声明的正文字节数。0 允许。不与 `body` 的字节长度对账。
    pub byte_count: u64,
    /// 调用方交来的工具正文。空串允许。估算只数其中的汉字、英文字母和 ASCII 数字。`Debug` 不打印，不进入 context 记录。
    pub body: String,
}

/// 发给模型的视图。P1 与 [`Transcript`] 的文字相同。
///
/// **作用**：`prepare` 和 `replay` 的成功结果。序列化成 Chat Completions 仍留在 `engine-custom`。
/// **边界**：恒等策略不改字段。`Debug` 只打印长度。
#[derive(Clone, PartialEq, Eq)]
pub struct View {
    /// 将要发给模型的系统提示。恒等策略下与 [`Transcript::system`] 相同。`Debug` 只打印字符数。
    pub system: String,
    /// 将要发给模型的首条用户消息。恒等策略下与 [`Transcript::user`] 相同。`Debug` 只打印字符数。
    pub user: String,
    /// 将要发给模型的各轮历史。恒等策略下与 [`Transcript::rounds`] 相同，包括工具正文。
    pub rounds: Vec<Round>,
}

impl View {
    /// 按恒等关系从事实抄出视图。
    ///
    /// **作用**：空链的 `prepare` 和 `identity@1` 的 `replay` 使用。
    /// **参数**：`transcript` 的正文被克隆。调用方仍持有原来的事实。
    /// **边界**：不删除、不替换任何字段。
    pub(crate) fn identity(transcript: &Transcript) -> Self {
        Self {
            system: transcript.system.clone(),
            user: transcript.user.clone(),
            rounds: transcript.rounds.clone(),
        }
    }
}

impl Transcript {
    /// 数下一次请求里会出现的全部可计数文字。
    ///
    /// **作用**：全量估算调用。不看各轮用量，也不决定用不用锚点。
    /// **边界**：工具结果的名字和参数不重复计数；它们已在 assistant 的工具调用里。`result_ref` 不计数。
    pub(crate) fn full_count(&self) -> Count {
        let mut count = count_text(&self.system).saturating_add(count_text(&self.user));
        for round in &self.rounds {
            count = count.saturating_add(round.count());
        }
        count
    }

    /// 锚点轮之后、下一次请求之前，服务端没有数过的文字。
    ///
    /// **作用**：锚点轮的新增内容，加上锚点轮之后每一轮的全部文字。
    /// **参数**：`index` 是锚点轮在 `rounds` 里的下标，必须小于 `rounds.len()`。
    /// **边界**：锚点轮是最后一轮时，结果与 [`Transcript::delta_count`] 相同。
    pub(crate) fn count_after(&self, index: usize) -> Count {
        let mut count = Self::delta_count(&self.rounds[index]);
        for round in &self.rounds[index + 1..] {
            count = count.saturating_add(round.count());
        }
        count
    }

    /// 只数这一轮响应之后新出现、当时还没被服务端计数的文字。
    ///
    /// **作用**：锚点估算把这段加到该轮的 `prompt_tokens + completion_tokens` 上。
    /// **参数**：`round` 是锚点轮，不一定是最后一轮。只读它的工具正文和 `trailing_user`；空串会计成 0。
    /// **边界**：不数 assistant 正文、思维链和工具调用参数。锚点之后的轮次由 [`Transcript::count_after`] 另数。
    pub(crate) fn delta_count(round: &Round) -> Count {
        let mut count = count_text(&round.trailing_user);
        for tool in &round.tools {
            count = count.saturating_add(count_text(&tool.body));
        }
        count
    }
}

impl Round {
    /// 这一轮在下一次请求里出现的全部文字。
    ///
    /// **作用**：全量估算，以及锚点轮之后各轮的估算。
    /// **边界**：assistant 正文、思维链、工具调用的 `id` / `name` / `arguments`、工具正文和 `trailing_user`。
    /// 工具结果的名字和参数已在工具调用里，`result_ref` 不计数。
    pub(crate) fn count(&self) -> Count {
        let mut count = count_text(&self.assistant.content);
        count = count.saturating_add(count_text(&self.assistant.reasoning));
        for call in &self.assistant.tool_calls {
            count = count.saturating_add(count_text(&call.id));
            count = count.saturating_add(count_text(&call.name));
            count = count.saturating_add(count_text(&call.arguments));
        }
        for tool in &self.tools {
            count = count.saturating_add(count_text(&tool.body));
        }
        count.saturating_add(count_text(&self.trailing_user))
    }
}

impl fmt::Debug for Transcript {
    /// 打印 system、user 的字符数和轮数。
    ///
    /// **作用**：测试失败时能看出形状。
    /// **参数**：`f` 是 `Debug` 的输出目标。只写入长度，不写入正文。
    /// **边界**：不打印 prompt、思维链、工具参数或工具正文。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcript")
            .field("system_chars", &self.system.chars().count())
            .field("user_chars", &self.user.chars().count())
            .field("rounds", &self.rounds.len())
            .finish()
    }
}

impl fmt::Debug for Round {
    /// 打印工具条数、用量和 `trailing_user` 的字符数。
    ///
    /// **作用**：跟在 [`Transcript`] 的 `Debug` 后面使用。
    /// **参数**：`f` 是 `Debug` 的输出目标。用量数字可以写入。
    /// **边界**：不打印 assistant 或工具正文。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Round")
            .field("tools", &self.tools.len())
            .field("usage", &self.usage)
            .field("trailing_user_chars", &self.trailing_user.chars().count())
            .finish()
    }
}

impl fmt::Debug for AssistantTurn {
    /// 打印正文字符数、思维链字符数和工具调用条数。
    ///
    /// **作用**：避免 `Debug` 把思维链带进日志。
    /// **参数**：`f` 是 `Debug` 的输出目标。只写入长度和条数。
    /// **边界**：不打印 `content`、`reasoning` 或参数原文。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantTurn")
            .field("content_chars", &self.content.chars().count())
            .field("reasoning_chars", &self.reasoning.chars().count())
            .field("tool_calls", &self.tool_calls.len())
            .finish()
    }
}

impl fmt::Debug for ToolCall {
    /// 打印调用 ID、工具名和参数字符数。
    ///
    /// **作用**：ID 和工具名不是正文，参数只留长度。
    /// **参数**：`f` 是 `Debug` 的输出目标。
    /// **边界**：不打印 `arguments`。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCall")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("arguments_chars", &self.arguments.chars().count())
            .finish()
    }
}

impl fmt::Debug for ToolResult {
    /// 打印调用 ID、工具名、引用、声明字节数和正文字符数。
    ///
    /// **作用**：`result_ref` 是引用，可以出现在日志里。
    /// **参数**：`f` 是 `Debug` 的输出目标。
    /// **边界**：不打印 `body` 或参数原文。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolResult")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("result_ref", &self.result_ref)
            .field("byte_count", &self.byte_count)
            .field("body_chars", &self.body.chars().count())
            .finish()
    }
}

impl fmt::Debug for View {
    /// 打印 system、user 的字符数和轮数。
    ///
    /// **作用**：比较恒等视图时不把正文打进失败日志。
    /// **参数**：`f` 是 `Debug` 的输出目标。只写入长度。
    /// **边界**：不打印 system、user 或轮次正文。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("View")
            .field("system_chars", &self.system.chars().count())
            .field("user_chars", &self.user.chars().count())
            .field("rounds", &self.rounds.len())
            .finish()
    }
}
