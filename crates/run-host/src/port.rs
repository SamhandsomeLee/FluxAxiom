use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_engine_api::{
    BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome, EngineObservation,
    HostPort, HostPortError, InputOutcome, InputRequest, ProposedEffect,
};
use tokio_util::sync::CancellationToken;

/// A1.1 交给 Engine 的 Host 端口。
/// 观测和原文还没有落点：不能假装已经持久，所以 `observe` / `put_blob` 拒绝。
pub(crate) struct A11HostPort {
    cancel: CancellationToken,
}

impl A11HostPort {
    pub(crate) fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl HostPort for A11HostPort {
    /// 还没有 events 表。观测返回成功会让 Engine 以为事实已写入。
    fn observe(&self, _observation: EngineObservation) -> Result<(), HostPortError> {
        Err(HostPortError::Unsupported)
    }

    /// 还没有内容寻址目录。原文返回引用会让后续事件指向空 blob。
    fn put_blob(&self, _bytes: Bytes, _media_type: &str) -> Result<BlobRef, HostPortError> {
        Err(HostPortError::Unsupported)
    }

    /// 不记账。批准一个并未写入的额度会在 A2 之前制造假预算。
    fn reserve(&self, _request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        Err(BudgetExhausted)
    }

    /// 本次 drive 持有的令牌。A1.1 没有 Ctrl-C，不会被 cancel。
    fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// P0a 不执行外部副作用。
    async fn propose_effect(&self, _effect: ProposedEffect) -> EffectOutcome {
        EffectOutcome::Unsupported
    }

    /// P0a 不向用户要额外输入。
    async fn request_input(&self, _request: InputRequest) -> InputOutcome {
        InputOutcome::Unsupported
    }
}
