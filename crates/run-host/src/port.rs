use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{AttemptStatus, RunId, RunStatus};
use fluxaxiom_engine_api::{
    BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome, EngineObservation,
    HostPort, HostPortError, InputOutcome, InputRequest, ProposedEffect,
};
use tokio_util::sync::CancellationToken;

use crate::blob;
use crate::error::HostError;
use crate::events;
use crate::host::Shared;
use crate::store;

/// 一次 `drive_run` 交给 Engine 的 [`HostPort`]。
///
/// **作用**：Engine 只能经这里提交 blob 和观测。`run_id` 在构造时固定。
/// **边界**：不写 `run.*` / `attempt.*`。观测在 Run 不是 `Running` 时返回 `Invalid`。
/// `reserve` 仍不记账。`propose_effect` / `request_input` 仍不支持。
pub(crate) struct DrivePort {
    shared: Arc<Shared>,
    run_id: RunId,
    cancel: CancellationToken,
}

impl DrivePort {
    pub(crate) fn new(shared: Arc<Shared>, run_id: RunId) -> Self {
        Self {
            shared,
            run_id,
            cancel: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl HostPort for DrivePort {
    /// **作用**：校验观测并写入一条 `model.call_completed` 或 `tool.call_completed`。
    /// **边界**：返回 `Ok` 时该 event 已 COMMIT。缺字段或 `byte_count` 对不上返回 `Invalid`，不写 event。
    /// 写失败返回 `Storage`，细节只进日志。
    fn observe(&self, observation: EngineObservation) -> Result<(), HostPortError> {
        observe_durable(&self.shared, &self.run_id, &observation).map_err(to_port_error)
    }

    /// **作用**：按内容保存原文并返回引用。
    /// **边界**：`media_type` 为空返回 `Invalid`，类型不入库。写失败返回 `Storage`。
    fn put_blob(&self, bytes: Bytes, media_type: &str) -> Result<BlobRef, HostPortError> {
        blob::put(&self.shared.root, &bytes, media_type).map_err(to_port_error)
    }

    /// **作用**：调用模型或工具前预留额度。
    /// **边界**：A1.2 不记账。调用即视为没有可用额度，避免批准一笔没写入的预算。
    fn reserve(&self, _request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        Err(BudgetExhausted)
    }

    /// **作用**：把这次 drive 的取消令牌交给 Engine，供它在 await 点观察。
    /// **边界**：令牌只属于这一次 drive。A1.2 没有 Ctrl-C，不会被 cancel。
    fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// **作用**：提议经 Effect Gateway 执行的外部副作用。
    /// **边界**：P0a 不执行外部副作用，恒返回 `Unsupported`。
    async fn propose_effect(&self, _effect: ProposedEffect) -> EffectOutcome {
        EffectOutcome::Unsupported
    }

    /// **作用**：向用户索取非审批输入。
    /// **边界**：P0a 不在 run 中途阻塞要输入，恒返回 `Unsupported`。
    async fn request_input(&self, _request: InputRequest) -> InputOutcome {
        InputOutcome::Unsupported
    }
}

/// 把一次观测写成 durable event。
///
/// **作用**：在 Run 与 Attempt 都处于 `Running` 时，用当前 Attempt 做 `subject_id` 写入一条模型或工具事件。
/// **边界**：先校验再开 `BEGIN IMMEDIATE`。校验失败不写行。返回 `Ok` 时事务已提交。不发 `run.*` / `attempt.*`。
pub(crate) fn observe_durable(
    shared: &Shared,
    run_id: &RunId,
    observation: &EngineObservation,
) -> Result<(), HostError> {
    let body = events::observation_body(&shared.root, observation)?;
    let mut connection = shared.conn.lock().map_err(|_| HostError::Poisoned)?;
    let transaction = store::immediate(&mut connection)?;
    let run = store::load_run(&transaction, run_id)?;
    let attempt = store::latest_attempt(&transaction, run_id)?;
    if run.status != RunStatus::Running || attempt.status != AttemptStatus::Running {
        return Err(HostError::Invalid("run is not running"));
    }
    let sequence = events::max_sequence(&transaction, run_id)? + 1;
    events::record(
        &transaction,
        run_id,
        sequence,
        attempt.attempt_id.as_str(),
        body,
    )?;
    transaction.commit()?;
    Ok(())
}

/// 把 Host 内部错误收成端口错误。
///
/// **作用**：让 Engine 只看见 `Invalid` 或 `Storage`。
/// **边界**：校验失败保持 `Invalid`，不写日志。写库或写文件失败抹成无载荷的 `Storage`，路径和 SQLite 原文只进日志。
fn to_port_error(error: HostError) -> HostPortError {
    match error {
        HostError::Invalid(_) => HostPortError::Invalid,
        other => {
            log::error!("host persist failed: {other}");
            HostPortError::Storage
        }
    }
}
