use fluxaxiom_contract::{ModelUsage, RunId};

use crate::workspace::WorkspaceRoot;

/// Engine 身份。Host 用它写入 Attempt 的 `engine_id` / `engine_version`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDescriptor {
    pub engine_id: String,
    pub version: String,
}

/// Host 启动一次 Engine 执行时传入的任务。
///
/// **作用**：`EngineAdapter::run` 的入参。`run_id` 已由 Host 创建。
/// **边界**：`input` 是用户任务文本，不是 blob 引用。`workspace` 是 Host 已经 `open` 过的授权根；Engine 不自己解析 `workspace_ref`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRunRequest {
    pub run_id: RunId,
    pub input: String,
    pub workspace: WorkspaceRoot,
}

/// Engine 交回的产物草稿。Host 负责落成 Artifact；Engine 不写存储。
///
/// **作用**：`body` 是完整正文（进 blob）；`summary` 是短摘要（进事件与索引）。
/// **边界**：P0a 由 Engine 取 `body` 首个非空行截断到 200 字符填 `summary`（A4b / ADR-023）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDraft {
    pub artifact_type: String,
    /// 短摘要，只进 `artifact.created` 与 Artifact 索引，不进 blob。
    pub summary: String,
    /// 完整正文；Host 只把它写成 blob。
    pub body: String,
}

impl ArtifactDraft {
    /// 用完整正文构造报告草稿，并按规则填 `summary`。
    ///
    /// **作用**：Engine 成功 / 部分报告的共用入口。
    /// **参数**：`body` 是交给 Host 写 blob 的全文。
    /// **边界**：`summary` 取首个非空行，截断到 200 字符；全文为空时 `summary` 也为空。
    #[must_use]
    pub fn report(body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            artifact_type: "report".to_owned(),
            summary: summarize_body(&body),
            body,
        }
    }
}

/// 从正文取事件用短摘要。
///
/// **作用**：与 [`ArtifactDraft::report`] 同一规则，供测试断言。
/// **边界**：跳过空白行；截断按 Unicode 标量，最多 200。
#[must_use]
pub fn summarize_body(body: &str) -> String {
    let line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    line.chars().take(200).collect()
}

/// 内容寻址原文的引用。事件只保存这个引用，不保存文件全文、prompt 或 `reasoning_content`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef(String);

impl BlobRef {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Engine 交给 Host 的可持久观测。Host 校验后写成 durable event。
///
/// **作用**：把模型或只读工具调用的摘要交给 Host。原文已经由 `put_blob` 存好，这里只带引用和 Host 编不了的数字。
///
/// **边界**：不能用来发 `run.*` / `attempt.*`。空字符串和空 `BlobRef` 视为缺字段，Host 返回 `Invalid` 且不写 event。
/// `latency_ms` 与 `duration_ms` 允许为 0。`byte_count` 为 0 表示交给模型的内容为空，不是未知长度。
/// `usage` 里缺失的 token 用契约的 `TokenCount::Unknown`，不要填 0 冒充。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineObservation {
    /// 一次模型调用完成。请求和响应原文在两个 blob 引用里，不在本结构里。
    ModelCall {
        /// 请求时的模型别名。空字符串是 `Invalid`。
        model_alias: String,
        /// 用量。缓存命中缺失时用 `Unknown`，不要编造数字。
        usage: ModelUsage,
        /// Provider 的 `finish_reason`。空字符串是 `Invalid`。
        finish_reason: String,
        /// 本次调用墙钟毫秒。0 合法。
        latency_ms: u64,
        /// 完整请求 blob。空引用是 `Invalid`。
        request_ref: BlobRef,
        /// 完整响应 blob，含 `reasoning_content`。空引用是 `Invalid`。
        response_ref: BlobRef,
    },
    /// 一次只读工具调用完成。结果原文在 `result_ref`，不在本结构里。
    ToolCall {
        /// 工具名。空字符串是 `Invalid`。
        tool_name: String,
        /// 规范化路径或查询。空字符串是 `Invalid`。只读工具才走这里，不是 Effect。
        normalized_path_or_query: String,
        /// 工具结果 blob。空引用是 `Invalid`。
        result_ref: BlobRef,
        /// 返回给模型的字节数，等于 `result_ref` 里那份内容的长度。0 表示这份内容为空。
        /// 不记录截断前的原始长度。
        byte_count: u64,
        /// 为 true 时，工具原始输出大于 `byte_count`；多出来的部分没有交给模型，也不在本事件里。
        truncated: bool,
        /// 工具耗时毫秒。0 合法。
        duration_ms: u64,
    },
}

/// 调用模型或工具前向 Host 预留的额度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetRequest {
    pub model_calls: u32,
    pub tool_calls: u32,
}

/// Host 已批准的本次预留。Engine 用完后再发起下一次 `reserve`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetLease {
    pub model_calls: u32,
    pub tool_calls: u32,
}

/// 预算已耗尽。Engine 应收尾并返回部分结果，由 Host 标为 `Failed(budget_exhausted)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExhausted;

/// Engine 提议的外部副作用。P0a 只保留签名；写文件等实现留到 P0b。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedEffect {
    pub tool_id: String,
}

/// `propose_effect` 的结果。P0a 只有 `Unsupported`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectOutcome {
    Unsupported,
}

/// Engine 向用户请求的非审批输入。P0a 只保留签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRequest {
    pub prompt: String,
}

/// `request_input` 的结果。P0a 只有 `Unsupported`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Unsupported,
}

/// Engine 未能完成时的原因。Host 用它决定 Run 的 `terminal_reason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    BudgetExhausted,
    EngineError,
    ProviderError,
}

/// Engine 一次 `run` 的返回值。Host 据此写入 Attempt 结束和 Run 终态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineOutcome {
    Completed {
        report: ArtifactDraft,
    },
    Failed {
        reason: FailureReason,
        partial: Option<ArtifactDraft>,
    },
    Cancelled {
        partial: Option<ArtifactDraft>,
    },
}
