use fluxaxiom_contract::RunId;

/// Engine 身份。Host 用它写入 Attempt 的 `engine_id` / `engine_version`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDescriptor {
    pub engine_id: String,
    pub version: String,
}

/// Host 启动一次 Engine 执行时传入的任务。
/// `run_id` 已由 Host 创建；`input` 是用户任务文本，不是 blob 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRunRequest {
    pub run_id: RunId,
    pub input: String,
}

/// Engine 交回的产物草稿。Host 负责落成 Artifact；Engine 不写存储。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDraft {
    pub artifact_type: String,
    pub summary: String,
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

/// Engine 交给 Host 的可持久观测。Host 校验后写入 durable event。
/// Engine 不能用它发送 `run.*` 或 `attempt.*`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineObservation {
    /// 一次模型调用完成。原文在 `request_ref` / `response_ref` 指向的 blob 里。
    ModelCall {
        model_alias: String,
        request_ref: BlobRef,
        response_ref: BlobRef,
    },
    /// 一次只读工具调用完成。结果原文在 `result_ref` 指向的 blob 里。
    ToolCall {
        tool_name: String,
        result_ref: BlobRef,
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
