use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use fluxaxiom_contract::{
    attempt_transition_allowed, run_transition_allowed, Attempt, AttemptId, AttemptOutcome,
    AttemptStatus, Budget, Run, RunId, RunStatus, Session, SessionId, TerminalReason,
};
use fluxaxiom_engine_api::{
    EngineAdapter, EngineDescriptor, EngineOutcome, EngineRunRequest, FailureReason, HostPort,
};
use rusqlite::Connection;

use crate::error::{is_constraint_violation, HostError};
use crate::port::A11HostPort;
use crate::store;
use crate::time::{lease_expires_at, now_utc};

/// 创建 Run 的 Host 受理参数。`request_id` 只在 Host 侧做幂等，不进入契约 `Run`。
pub struct CreateRun {
    /// 同一值再次创建时返回已有 Run，不另插一行。
    pub request_id: String,
    /// 省略则 Run 不挂 Session；传入的 Session 必须已经存在。
    pub session_id: Option<SessionId>,
    /// A1.1 暂存任务文本，并原样交给 Engine。A1.2 改为 blob 引用。
    pub input_ref: String,
    /// 授权工作区标识。A1.1 只保存字符串，不检查路径。
    pub workspace_ref: String,
    /// 写入 Run.budget。A1.1 不按它扣减；端口上的 `reserve` 直接视为耗尽。
    pub budget: Budget,
    /// 写入 Attempt 的 `engine_id` / `engine_version`。
    pub engine: EngineDescriptor,
}

/// 一次创建得到的 Run 与其 Attempt。A1.1 每个 Run 只有一个 Attempt。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRun {
    pub run: Run,
    pub attempt: Attempt,
}

/// Session / Run / Attempt 的权威写入点。Engine 不持有这个类型。
///
/// 进程内用互斥锁串行化同一连接；跨进程靠 WAL 与 `busy_timeout` 等待写锁。
pub struct RunHost {
    conn: Mutex<Connection>,
}

impl RunHost {
    /// 打开或创建 SQLite 文件，启用 WAL，并跑完待执行的迁移。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HostError> {
        let mut connection = store::connect(path.as_ref())?;
        store::migrate(&mut connection)?;
        Ok(Self {
            conn: Mutex::new(connection),
        })
    }

    /// 由 Host 创建 Session。Engine 看不到这张表。
    pub fn create_session(&self) -> Result<Session, HostError> {
        let session = Session {
            session_id: SessionId::generate(),
            created_at: now_utc(),
        };
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        store::insert_session(&transaction, &session)?;
        transaction.commit()?;
        Ok(session)
    }

    /// 创建 Accepted 的 Run 和 Starting 的 Attempt。
    /// 租约此刻写入，但本阶段不根据它判活。
    pub fn create_run(&self, request: CreateRun) -> Result<CreatedRun, HostError> {
        validate_create(&request)?;
        let mut connection = self.lock()?;
        if let Some(created) = load_created(&connection, &request.request_id)? {
            return Ok(created);
        }
        if let Some(session_id) = &request.session_id {
            store::load_session(&connection, session_id)?;
        }
        let prepared = build_created(request)?;
        let transaction = connection.transaction()?;
        match store::insert_created(
            &transaction,
            &prepared.record.run,
            &prepared.record.attempt,
            &prepared.request_id,
        ) {
            Ok(()) => {
                transaction.commit()?;
                Ok(prepared.record)
            }
            Err(HostError::Sqlite(error)) if is_constraint_violation(&error) => {
                // 并发下两个进程可能同时插入同一 request_id。回滚后读赢家那一行。
                transaction.rollback()?;
                load_created(&connection, &prepared.request_id)?.ok_or(HostError::Corrupt(
                    "unique request_id conflict but row missing",
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// 按主键读已持久化的 Session。
    pub fn get_session(&self, session_id: &SessionId) -> Result<Session, HostError> {
        let connection = self.lock()?;
        store::load_session(&connection, session_id)
    }

    /// 按主键读已持久化的 Run。
    pub fn get_run(&self, run_id: &RunId) -> Result<Run, HostError> {
        let connection = self.lock()?;
        store::load_run(&connection, run_id)
    }

    /// 按主键读已持久化的 Attempt。
    pub fn get_attempt(&self, attempt_id: &AttemptId) -> Result<Attempt, HostError> {
        let connection = self.lock()?;
        store::load_attempt(&connection, attempt_id)
    }

    /// 按幂等键读取。没有这条请求时返回 `NotFound`。
    pub fn get_by_request_id(&self, request_id: &str) -> Result<CreatedRun, HostError> {
        let connection = self.lock()?;
        load_created(&connection, request_id)?.ok_or(HostError::NotFound)
    }

    /// 经 `HostPort` 跑一次 Engine，再根据 `EngineOutcome` 写入终态。
    ///
    /// 不持锁越过 `.await`。报告草稿本阶段丢弃，等 A1.2 与 `artifact.created` 同事务写入。
    pub async fn drive_run(
        &self,
        run_id: &RunId,
        engine: &dyn EngineAdapter,
    ) -> Result<EngineOutcome, HostError> {
        let request = self.prepare_drive(run_id)?;
        let host: Arc<dyn HostPort> = Arc::new(A11HostPort::new());
        let outcome = engine.run(request, host).await;
        self.finish_drive(run_id, &outcome)?;
        Ok(outcome)
    }

    fn prepare_drive(&self, run_id: &RunId) -> Result<EngineRunRequest, HostError> {
        let mut connection = self.lock()?;
        let mut run = store::load_run(&connection, run_id)?;
        let mut attempt = store::latest_attempt(&connection, run_id)?;
        if !run_transition_allowed(run.status, RunStatus::Running)
            || !attempt_transition_allowed(attempt.status, AttemptStatus::Running)
        {
            return Err(HostError::Invalid("run is not startable"));
        }
        let now = now_utc();
        run.status = RunStatus::Running;
        run.updated_at = now;
        attempt.status = AttemptStatus::Running;
        let input = run.input_ref.clone();
        let transaction = connection.transaction()?;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        transaction.commit()?;
        Ok(EngineRunRequest {
            run_id: run.run_id,
            input,
        })
    }

    fn finish_drive(&self, run_id: &RunId, outcome: &EngineOutcome) -> Result<(), HostError> {
        let mut connection = self.lock()?;
        let mut run = store::load_run(&connection, run_id)?;
        let mut attempt = store::latest_attempt(&connection, run_id)?;
        apply_outcome(&mut run, &mut attempt, outcome)?;
        let transaction = connection.transaction()?;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        transaction.commit()?;
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, HostError> {
        self.conn.lock().map_err(|_| HostError::Poisoned)
    }
}

struct PreparedCreate {
    request_id: String,
    record: CreatedRun,
}

fn validate_create(request: &CreateRun) -> Result<(), HostError> {
    if request.request_id.is_empty() {
        return Err(HostError::Invalid("request_id must not be empty"));
    }
    if request.input_ref.is_empty() {
        return Err(HostError::Invalid("input_ref must not be empty"));
    }
    if request.workspace_ref.is_empty() {
        return Err(HostError::Invalid("workspace_ref must not be empty"));
    }
    if request.engine.engine_id.is_empty() || request.engine.version.is_empty() {
        return Err(HostError::Invalid("engine identity must not be empty"));
    }
    Ok(())
}

fn build_created(request: CreateRun) -> Result<PreparedCreate, HostError> {
    let created_at = now_utc();
    let run = Run {
        run_id: RunId::generate(),
        session_id: request.session_id,
        status: RunStatus::Accepted,
        terminal_reason: None,
        input_ref: request.input_ref,
        workspace_ref: request.workspace_ref,
        budget: request.budget,
        created_at: created_at.clone(),
        updated_at: created_at,
    };
    let attempt = Attempt {
        attempt_id: AttemptId::generate(),
        run_id: run.run_id.clone(),
        engine_id: request.engine.engine_id,
        engine_version: request.engine.version,
        // 进程身份，不是权限。A3 的 status 用它避免把仍在跑的进程标成孤儿。
        owner: std::process::id().to_string(),
        lease_expires_at: lease_expires_at(),
        status: AttemptStatus::Starting,
        outcome: None,
        error_reason: None,
    };
    if !run.invariants_hold() || !attempt.invariants_hold() {
        return Err(HostError::Invalid("created records violate invariants"));
    }
    Ok(PreparedCreate {
        request_id: request.request_id,
        record: CreatedRun { run, attempt },
    })
}

fn load_created(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<CreatedRun>, HostError> {
    let Some((run, attempt)) = store::load_created_by_request_id(connection, request_id)? else {
        return Ok(None);
    };
    Ok(Some(CreatedRun { run, attempt }))
}

fn apply_outcome(
    run: &mut Run,
    attempt: &mut Attempt,
    outcome: &EngineOutcome,
) -> Result<(), HostError> {
    let (status, reason, attempt_outcome, error_reason) = match outcome {
        EngineOutcome::Completed { .. } => (
            RunStatus::Completed,
            TerminalReason::Completed,
            AttemptOutcome::Completed,
            None,
        ),
        EngineOutcome::Failed { reason, .. } => {
            let (terminal, error_reason) = match reason {
                FailureReason::BudgetExhausted => {
                    (TerminalReason::BudgetExhausted, "budget_exhausted")
                }
                FailureReason::EngineError => (TerminalReason::EngineError, "engine_error"),
                FailureReason::ProviderError => (TerminalReason::ProviderError, "provider_error"),
            };
            (
                RunStatus::Failed,
                terminal,
                AttemptOutcome::Failed,
                Some(error_reason.to_owned()),
            )
        }
        EngineOutcome::Cancelled { .. } => (
            RunStatus::Cancelled,
            TerminalReason::Cancelled,
            AttemptOutcome::Cancelled,
            None,
        ),
    };
    if !run_transition_allowed(run.status, status)
        || !attempt_transition_allowed(attempt.status, AttemptStatus::Ended)
    {
        return Err(HostError::Invalid("run is not finishable"));
    }
    run.status = status;
    run.terminal_reason = Some(reason);
    run.updated_at = now_utc();
    attempt.status = AttemptStatus::Ended;
    attempt.outcome = Some(attempt_outcome);
    attempt.error_reason = error_reason;
    if !run.invariants_hold() || !attempt.invariants_hold() {
        return Err(HostError::Invalid("terminal records violate invariants"));
    }
    Ok(())
}

#[cfg(test)]
impl RunHost {
    pub(crate) fn journal_mode(&self) -> Result<String, HostError> {
        let connection = self.lock()?;
        store::journal_mode(&connection)
    }

    pub(crate) fn busy_timeout_ms(&self) -> Result<i64, HostError> {
        let connection = self.lock()?;
        store::busy_timeout_ms(&connection)
    }
}

#[cfg(test)]
mod tests {
    use super::RunHost;

    #[test]
    fn opens_with_wal_and_five_second_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.db");
        let host = RunHost::open(&path).unwrap();
        assert!(host.journal_mode().unwrap().eq_ignore_ascii_case("wal"));
        assert_eq!(host.busy_timeout_ms().unwrap(), 5_000);
    }
}
