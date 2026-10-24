use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::{AttemptStatus, Budget, RunId, RunStatus};
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

/// 本次 drive 还剩的模型/工具调用次数。
///
/// **作用**：`reserve` 在内存里扣减。上限来自构造时的 `Run.budget`，不持久化已用量。
#[derive(Debug, Clone, Copy)]
struct Remaining {
    model_calls: u32,
    tool_calls: u32,
}

/// 一次 `drive_run` 交给 Engine 的 [`HostPort`]。
///
/// **作用**：Engine 只能经这里提交 blob、观测和预算预留。`run_id` 与剩余额度在构造时固定。
/// **边界**：不写 `run.*` / `attempt.*`。观测在 Run 不是 `Running` / `Cancelling` 时返回 `Invalid`；`reserve` 只在 `Running` 发 lease，
/// 取消后的 Engine 不该再开新调用（`finish_drive` 无论 Engine 交回什么都会把 `Cancelling` 的 Run 收成 `Cancelled`）。
/// `reserve` 只在本次 drive 的内存里计数。`propose_effect` / `request_input` 仍不支持。
pub(crate) struct DrivePort {
    shared: Arc<Shared>,
    run_id: RunId,
    cancel: CancellationToken,
    remaining: Mutex<Remaining>,
}

impl DrivePort {
    /// 用本次 drive 的 Run 预算上限与取消令牌构造端口。
    ///
    /// **作用**：把 Host 共享状态收成 Engine 可见的 `HostPort`；仅由 `drive_run` 调用。
    /// **参数**：`budget` 是 `prepare_drive` 时读到的 `Run.budget`。`cancel` 与 `RunHost` 登记的是同一把，供 CLI `request_cancel` 触发。
    /// **边界**：已用量不入库。同一次 `drive_run` 共用这一把计数器；下一次 drive 会新建端口。
    pub(crate) fn new(
        shared: Arc<Shared>,
        run_id: RunId,
        budget: Budget,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            shared,
            run_id,
            cancel,
            remaining: Mutex::new(Remaining {
                model_calls: budget.max_model_calls,
                tool_calls: budget.max_tool_calls,
            }),
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

    /// **作用**：调用模型或工具前预留额度。成功时 `BudgetLease` 与请求的两个字段相等。
    ///
    /// **参数**：`request` 是本次要扣的次数。`{0,0}` 允许，返回 `{0,0}`，不改计数器；余额为 0 时也如此。
    /// **边界**：`BudgetExhausted` 同时表示「额度不够」和「Run / Attempt 已不在 `Running`，不再发 lease」。
    /// 本阶段不扩展 `HostPort` 去区分这两种情况。不做部分批准：两个维度要么都扣，要么都不扣，用 `checked_sub`。
    /// 锁内不 `await`。没有归还接口：调用失败后已扣的额度不退回，Engine 在调用前预留。
    /// 已用量只在本次 drive 的内存里，同一个 Run 再次 drive 会从上限重新开始，见执行方案 §4.7.4 第 3 项。
    fn reserve(&self, request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        let connection = self.shared.conn.lock().map_err(|_| BudgetExhausted)?;
        let run = store::load_run(&connection, &self.run_id).map_err(|_| BudgetExhausted)?;
        let attempt =
            store::latest_attempt(&connection, &self.run_id).map_err(|_| BudgetExhausted)?;
        if run.status != RunStatus::Running || attempt.status != AttemptStatus::Running {
            return Err(BudgetExhausted);
        }
        if request.model_calls == 0 && request.tool_calls == 0 {
            return Ok(BudgetLease {
                model_calls: 0,
                tool_calls: 0,
            });
        }
        let mut remaining = self.remaining.lock().map_err(|_| BudgetExhausted)?;
        let model_calls = remaining
            .model_calls
            .checked_sub(request.model_calls)
            .ok_or(BudgetExhausted)?;
        let tool_calls = remaining
            .tool_calls
            .checked_sub(request.tool_calls)
            .ok_or(BudgetExhausted)?;
        remaining.model_calls = model_calls;
        remaining.tool_calls = tool_calls;
        Ok(BudgetLease {
            model_calls: request.model_calls,
            tool_calls: request.tool_calls,
        })
    }

    /// **作用**：把这次 drive 的取消令牌交给 Engine，供它在 await 点观察。
    /// **边界**：令牌只属于这一次 drive。由 CLI 经 `RunHost::request_cancel` 触发（A3b）。
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
/// **作用**：在 Run 为 `Running` 或 `Cancelling`、Attempt 为 `Running` 时，用当前 Attempt 做 `subject_id` 写入一条模型或工具事件。
/// **边界**：先校验再开 `BEGIN IMMEDIATE`。校验失败不写行。返回 `Ok` 时事务已提交。不发 `run.*` / `attempt.*`。
/// `Cancelling` 也接受：被 Ctrl-C 打断的那次调用已经花了钱，Engine 收尾时提交的 `model.call_completed` / `tool.call_completed`
/// 带着用量，丢掉会让已取消 Run 的事件日志和用量记账不完整。
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
    if !matches!(run.status, RunStatus::Running | RunStatus::Cancelling)
        || attempt.status != AttemptStatus::Running
    {
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
