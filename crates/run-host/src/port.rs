use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_engine_api::{
    BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome, EngineObservation,
    HostPort, HostPortError, InputOutcome, InputRequest, ProposedEffect,
};
use tokio_util::sync::CancellationToken;

/// A1.1 阶段交给 Engine 的 [`HostPort`] 实现。
///
/// **作用**：在 `RunHost::drive_run` 里以 `Arc<dyn HostPort>` 注入，供 Engine 在 `run` 期间回调 Host，
/// 而不让 Engine 直接访问 SQLite 或改写 Session / Run / Attempt。
///
/// **执行边界（本类型不负责）**：
/// - Run / Attempt 状态迁移、`run.*` / `attempt.*` 类 durable event：由 `RunHost::prepare_drive` /
///   `finish_drive` 在持锁与事务内完成。
/// - 模型 / 工具观测、`artifact.created`、内容寻址 blob：A1.2 起在本 trait 的其它实现中落地（ADR-014）。
/// - 预算记账、租约续期 / 过期扫描、Ctrl-C 取消：更后续里程碑。
///
/// **本阶段策略**：除提供可克隆的取消令牌外，凡需要「先持久再返回 Ok」的路径一律显式失败，
/// 避免 Engine 误以为观测或 blob 已 durable。
pub(crate) struct A11HostPort {
    cancel: CancellationToken,
}

impl A11HostPort {
    /// 为单次 `drive_run` 构造独立端口；令牌生命周期与该次 Engine 调用绑定。
    pub(crate) fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }
}

/// A1.1 对 [`HostPort`] 的桩实现：满足 trait 与 `dyn HostPort` 分发，不假装已有 A1.2+ 存储能力。
///
/// 同步方法在 Engine 线程上直接返回；异步方法无 I/O，仅返回阶段常量。
/// 所有方法均不访问 `RunHost` 或数据库。
#[async_trait]
impl HostPort for A11HostPort {
    /// **trait 语义**：校验观测并写入 durable fact（含 sequence），成功才返回 `Ok(())`。
    ///
    /// **A1.1 边界**：无 `events` 表；返回 `Ok` 会误导 Engine。恒 `Err(Unsupported)`，不读写字段。
    /// `Invalid` / `Storage` 留给已落地存储的实现；本桩还没有存储，不能把「没实现」说成写失败。
    fn observe(&self, _observation: EngineObservation) -> Result<(), HostPortError> {
        Err(HostPortError::Unsupported)
    }

    /// **trait 语义**：内容寻址保存原文，返回供事件引用的 [`BlobRef`]；失败时不应产出可用引用。
    ///
    /// **A1.1 边界**：无 blob 存储；假引用会导致后续事件指向空内容。恒 `Err(Unsupported)`。
    /// 磁盘失败才是 `Storage`；本桩还没有目录，不使用那个变体。
    fn put_blob(&self, _bytes: Bytes, _media_type: &str) -> Result<BlobRef, HostPortError> {
        Err(HostPortError::Unsupported)
    }

    /// **trait 语义**：调用模型 / 工具前预留额度；耗尽时 Engine 应收尾并上报部分结果。
    ///
    /// **A1.1 边界**：无预算表与 lease 持久化；批准额度会产生假账。恒 `Err(BudgetExhausted)`。
    fn reserve(&self, _request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        Err(BudgetExhausted)
    }

    /// **trait 语义**：返回本次 Run 的取消令牌；Host 在 Ctrl-C 等场景下 `cancel()`，Engine 在 await 点协作退出。
    ///
    /// **A1.1 边界**：仅返回本 `drive` 实例持有的令牌；未接全局中断，默认不会被 cancel。允许 clone 给 Engine 任务。
    fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// **trait 语义**：提议经 Effect Gateway 执行的外部副作用（P0b 及以后）。
    ///
    /// **P0a 边界**：只读 CLI，无 Gateway。恒 [`EffectOutcome::Unsupported`]，不触达文件系统或网络。
    async fn propose_effect(&self, _effect: ProposedEffect) -> EffectOutcome {
        EffectOutcome::Unsupported
    }

    /// **trait 语义**：向用户索取非审批类输入（交互式补全等）。
    ///
    /// **P0a 边界**：CLI 不在 run 中途阻塞要输入。恒 [`InputOutcome::Unsupported`]。
    async fn request_input(&self, _request: InputRequest) -> InputOutcome {
        InputOutcome::Unsupported
    }
}
