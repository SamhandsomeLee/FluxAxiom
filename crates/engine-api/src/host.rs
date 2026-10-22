use async_trait::async_trait;
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::types::{
    BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome, EngineObservation,
    InputOutcome, InputRequest, ProposedEffect,
};

/// Host 端口上的同步调用失败。三种含义不能混用，否则 Engine 会走错收尾路径。
///
/// **作用**：告诉 Engine 这次 `observe` / `put_blob` 为什么没有成功。
///
/// **边界**：无载荷，保持 `Copy`。SQLite 原文和路径只写 Host 日志，不放进这个值，避免把存储细节带给 Engine。
/// Engine 对 `Invalid` 和 `Storage` 都收尾并交回失败，不解析错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPortError {
    /// 本阶段没有这个能力。`propose_effect` / `request_input` 用各自的 outcome；
    /// `observe` / `put_blob` 在存储落地前也返回它。不要用它表示写失败。
    Unsupported,
    /// 观测没通过校验，event 尚未写入。缺字段、空字符串、空 `BlobRef` 走这里。
    Invalid,
    /// 能力已经落地，但这次写库或写 blob 失败。忙超时、唯一约束、磁盘错误都归这一类。
    Storage,
}

impl std::fmt::Display for HostPortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "host port method is unsupported in this phase",
            Self::Invalid => "host port observation failed validation",
            Self::Storage => "host port failed to persist",
        })
    }
}

impl std::error::Error for HostPortError {}

/// Engine 向 Host 提交观测、blob 和预算的唯一入口。
/// Engine 不写数据库，也不发 `run.*` / `attempt.*`。Run 终态由 Host 根据 `EngineOutcome` 决定。
#[async_trait]
pub trait HostPort: Send + Sync {
    /// 提交一次模型或工具调用观测。
    ///
    /// **作用**：校验后写入 durable event。返回 `Ok` 时该 event 已经 COMMIT。
    ///
    /// **边界**：缺字段返回 `Invalid`，不写 event。写库失败返回 `Storage`。
    /// 存储还没落地的实现返回 `Unsupported`，不得假装成功。
    fn observe(&self, observation: EngineObservation) -> Result<(), HostPortError>;

    /// 按内容保存原文（请求、响应、工具结果、`reasoning_content`），返回引用供事件使用。
    ///
    /// **作用**：先落文件再返回 `BlobRef`。事件只保存这个引用。
    ///
    /// **边界**：写文件失败返回 `Storage`，不得返回一个读不回的引用。
    /// 目录还没落地的实现返回 `Unsupported`。blob 不在 SQLite 事务里。
    fn put_blob(&self, bytes: Bytes, media_type: &str) -> Result<BlobRef, HostPortError>;

    /// 在调用模型或工具前预留额度。耗尽时返回 `BudgetExhausted`，Engine 应收尾并交回部分结果。
    fn reserve(&self, request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted>;

    /// 本次 Run 的取消令牌。Engine 在下一个 await 点检查；Ctrl-C 由 Host 触发。
    fn cancellation(&self) -> CancellationToken;

    /// 提议外部副作用。P0a 只保留签名，返回 `Unsupported`；P0b 才走 Effect Gateway。
    async fn propose_effect(&self, effect: ProposedEffect) -> EffectOutcome;

    /// 向用户请求非审批输入。P0a 只保留签名，返回 `Unsupported`。
    async fn request_input(&self, request: InputRequest) -> InputOutcome;
}
