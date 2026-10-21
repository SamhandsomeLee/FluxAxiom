use async_trait::async_trait;
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::types::{
    BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome, EngineObservation,
    InputOutcome, InputRequest, ProposedEffect,
};

/// Host 端口上的同步调用失败。P0a 的桩实现用 `Unsupported` 表示尚未落地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPortError {
    Unsupported,
}

impl std::fmt::Display for HostPortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host port method is unsupported in this phase")
    }
}

impl std::error::Error for HostPortError {}

/// Engine 向 Host 提交观测、blob 和预算的唯一入口。
/// Engine 不写数据库，也不发 `run.*` / `attempt.*`。Run 终态由 Host 根据 `EngineOutcome` 决定。
#[async_trait]
pub trait HostPort: Send + Sync {
    /// 提交一次模型或工具调用观测。返回前 Host 已校验并写入 durable event。
    fn observe(&self, observation: EngineObservation) -> Result<(), HostPortError>;

    /// 按内容保存原文（请求、响应、工具结果、`reasoning_content`），返回引用供事件使用。
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
