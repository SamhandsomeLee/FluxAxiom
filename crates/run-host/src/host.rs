use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fluxaxiom_contract::{
    attempt_transition_allowed, run_transition_allowed, Artifact, ArtifactId, Attempt, AttemptId,
    AttemptOutcome, AttemptStatus, Budget, Event, Run, RunId, RunSnapshot, RunStatus, Session,
    SessionId, TerminalReason, SCHEMA_VERSION,
};
use fluxaxiom_engine_api::{
    ArtifactDraft, BlobRef, EngineAdapter, EngineDescriptor, EngineOutcome, EngineRunRequest,
    FailureReason, HostPort, WorkspaceRoot,
};
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::blob;
use crate::error::{is_constraint_violation, HostError};
use crate::events;
use crate::port::DrivePort;
use crate::store;
use crate::time::{lease_expires_at, lease_is_expired, now_utc, LEASE_RENEW_INTERVAL_SECS};

/// 进程内共享的库连接和数据目录。
///
/// **作用**：`RunHost` 与 `DrivePort` 共用同一把锁和同一个 blob 根目录。
/// **边界**：锁只串行化本进程。跨进程仍靠 SQLite 的 WAL 和 `busy_timeout`。
/// `active_cancels` 只登记本进程正在 `drive_run` 的令牌，供 `request_cancel` / `orphan_drive` 触发。
pub(crate) struct Shared {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) root: PathBuf,
    pub(crate) active_cancels: Mutex<HashMap<String, CancellationToken>>,
    #[cfg(test)]
    fail_prepare: AtomicBool,
    #[cfg(test)]
    fail_finish: AtomicBool,
}

/// 创建 Run 的 Host 受理参数。`request_id` 只在 Host 侧做幂等，不进入契约 `Run`。
pub struct CreateRun {
    /// 同一值再次创建时返回已有 Run，不另插一行。
    pub request_id: String,
    /// 省略则 Run 不挂 Session；传入的 Session 必须已经存在。
    pub session_id: Option<SessionId>,
    /// 调用方传入的任务文本。Host 先把它写成 blob，库存的 `Run.input_ref` 是 `sha256:` 引用。
    pub input_ref: String,
    /// 授权工作区目录路径，可以是相对路径。`create_run` 会 `WorkspaceRoot::open` 它并把**规范化后的绝对路径**存进 `Run.workspace_ref`，所以这里的相对路径只按创建时的进程当前目录解析一次。
    /// 目录不存在、不是目录或路径不是 UTF-8 时 `create_run` 返回 `Invalid`，不建 Run。
    pub workspace_ref: String,
    /// 写入 Run.budget。`prepare_drive` 把它拷进本次 `DrivePort` 的内存计数上限；已用量不入库。
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

/// Session、Run、Attempt 和 durable event 的权威写入点。Engine 不持有这个类型。
///
/// **作用**：进程内用互斥锁串行化同一连接；跨进程靠 WAL 与 `busy_timeout` 等待写锁。
/// **边界**：写 event 的事务使用 `BEGIN IMMEDIATE`。blob 文件不在这些事务里。
/// `Clone` 只增加 `Arc` 计数，供 CLI 在 drive 同时监听 Ctrl-C。
#[derive(Clone)]
pub struct RunHost {
    shared: Arc<Shared>,
}

impl RunHost {
    /// 打开或创建 Host 数据目录。
    ///
    /// **作用**：在目录里建 `host.db`，启用 WAL，跑完迁移，并准备 `blobs/`。打开后扫描租约已过期的 drive（A3b）。
    /// **边界**：参数是目录，不是数据库文件。blob 放在 `blobs/ab/cd/<hex>`。
    /// 取舍：扫描挂在 `open` 上，这样任一 `flux` 子命令（含只读 `status`）都会收尸，符合 §4.8「任何 flux 命令启动时」。
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, HostError> {
        install_host_log();
        let root = directory.as_ref();
        std::fs::create_dir_all(root.join("blobs").join("tmp"))?;
        let mut connection = store::connect(&root.join("host.db"))?;
        store::migrate(&mut connection)?;
        let host = Self {
            shared: Arc::new(Shared {
                conn: Mutex::new(connection),
                root: root.to_path_buf(),
                active_cancels: Mutex::new(HashMap::new()),
                #[cfg(test)]
                fail_prepare: AtomicBool::new(false),
                #[cfg(test)]
                fail_finish: AtomicBool::new(false),
            }),
        };
        // 收尸失败不能让只读命令（如 `status`）也打不开库：记日志，下次 open 再试。
        if let Err(error) = host.reap_expired_leases() {
            log::error!("reaping expired leases failed: {error}");
        }
        Ok(host)
    }

    /// CLI 请求取消正在 drive 的 Run。
    ///
    /// **作用**：若仍为 `Running`，同事务写入 `Cancelling` 与 `run.cancelling`，并触发本进程登记的取消令牌。
    /// **参数**：`run_id` 必须已存在；不存在时下层返回 `NotFound`。
    /// **边界**：`Accepted` 且本进程的 `drive_run` 已登记令牌（正在进入 `Running`）时，只触发令牌：Engine 第一次检查就会交回 `Cancelled`。
    /// `Accepted` 但没有本进程的 drive 返回 `Invalid`（尚未 drive）。已是 `Cancelling` 或终态时不改库，但仍触发本进程令牌，
    /// 以便别的 `RunHost` 已收尸后本进程 drive 能从 await 醒来。后到的 cancel 不覆盖先提交的终态。
    pub fn request_cancel(&self, run_id: &RunId) -> Result<(), HostError> {
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let mut run = store::load_run(&transaction, run_id)?;
        match run.status {
            // 仍触发本进程令牌：扫描可能已在别的 `RunHost` 上收尸，本进程 drive 还在 await。
            RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Cancelled
            | RunStatus::Cancelling => {
                drop(transaction);
                self.trigger_cancel_token(run_id);
                return Ok(());
            }
            RunStatus::Accepted => {
                drop(transaction);
                return if self.trigger_cancel_token(run_id) {
                    Ok(())
                } else {
                    Err(HostError::Invalid("run has not started"))
                };
            }
            RunStatus::Running => {}
        }
        if !run_transition_allowed(run.status, RunStatus::Cancelling) {
            return Err(HostError::Invalid("run is not cancellable"));
        }
        run.status = RunStatus::Cancelling;
        run.updated_at = now_utc();
        let attempt = store::latest_attempt(&transaction, run_id)?;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        let sequence = events::max_sequence(&transaction, run_id)? + 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::cancelling(),
        )?;
        transaction.commit()?;
        self.trigger_cancel_token(run_id);
        Ok(())
    }

    /// 扫描并收尾租约已过期的 `Running` / `Cancelling` drive。
    ///
    /// **作用**：对每个过期候选调用 [`Self::orphan_drive`]；返回成功收尸的个数。`RunHost::open` 会调用一次。
    /// **边界**：只处理 `lease_expires_at <= now` 的 Attempt。未过期的 Running 不动，避免误杀仍在跑的进程。
    /// 单个 Run 收尸失败只记日志并继续，不中断扫描；只有列候选失败才返回 `Err`。
    pub fn reap_expired_leases(&self) -> Result<u32, HostError> {
        let now = now_utc();
        let candidates = {
            let connection = self.lock()?;
            store::list_active_drive_attempts(&connection)?
        };
        let mut reaped = 0u32;
        for (run, attempt) in candidates {
            if !lease_is_expired(&attempt.lease_expires_at, &now) {
                continue;
            }
            // 一个坏 Run 不能挡住后面的过期 Run：记日志，继续。
            match self.orphan_drive(&run.run_id) {
                Ok(true) => reaped += 1,
                Ok(false) => {}
                Err(error) => log::error!("orphaning {} failed: {error}", run.run_id.as_str()),
            }
        }
        Ok(reaped)
    }

    /// 把租约改到过去（集成测试用），便于断言孤儿扫描。
    ///
    /// **作用**：供 `tests/interrupt.rs` 构造「租约已过期仍 Running」的局面。
    /// **参数**：`run_id` 的最新 Attempt 会被写成 `2000-01-01T00:00:00Z`。
    /// **边界**：不是生产路径；正式续期只经 `drive_run` 内的定时器。
    pub fn expire_lease_for_test(&self, run_id: &RunId) -> Result<(), HostError> {
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        attempt.lease_expires_at = fluxaxiom_contract::Timestamp::parse("2000-01-01T00:00:00Z")
            .map_err(|_| HostError::Corrupt("test timestamp"))?;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        transaction.commit()?;
        Ok(())
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

    /// 创建 Accepted 的 Run、Starting 的 Attempt，并写入 `run.accepted`。
    ///
    /// **作用**：同一 `BEGIN IMMEDIATE` 事务提交状态行和 event。任务文本先落成 blob。
    /// **边界**：blob 的 fsync 发生在连接锁之外。租约此刻写入，但不续期、不扫描过期。重复 `request_id` 返回已有 Run，此时不重新检查工作区，也不采用这次传入的 `workspace_ref`。
    /// 工作区在这里就被打开并规范化：授权发生在创建时，库存的路径是绝对路径，`prepare_drive` 不会因为驱动进程的当前目录不同而打开另一个目录。目录打不开时不建 Run，也不写 blob。
    pub fn create_run(&self, mut request: CreateRun) -> Result<CreatedRun, HostError> {
        validate_create(&request)?;
        {
            let connection = self.lock()?;
            if let Some(created) = load_created(&connection, &request.request_id)? {
                return Ok(created);
            }
            if let Some(session_id) = &request.session_id {
                store::load_session(&connection, session_id)?;
            }
        }
        request.workspace_ref = canonical_workspace_ref(&request.workspace_ref)?;
        let text = std::mem::take(&mut request.input_ref);
        let stored = blob::put(&self.shared.root, text.as_bytes(), "text/plain")?;
        request.input_ref = stored.as_str().to_owned();
        let prepared = build_created(request)?;
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        match store::insert_created(
            &transaction,
            &prepared.record.run,
            &prepared.record.attempt,
            &prepared.request_id,
        ) {
            Ok(()) => {
                let sequence = events::max_sequence(&transaction, &prepared.record.run.run_id)? + 1;
                events::record(
                    &transaction,
                    &prepared.record.run.run_id,
                    sequence,
                    prepared.record.run.run_id.as_str(),
                    events::accepted(),
                )?;
                transaction.commit()?;
                Ok(prepared.record)
            }
            Err(HostError::Sqlite(error)) if is_constraint_violation(&error) => {
                // 并发下两个进程可能同时插入同一 request_id。回滚后读赢家那一行。
                // 这次先写的 blob 若无人引用，留在目录里。P0 不回收。
                transaction.rollback()?;
                load_created(&connection, &prepared.request_id)?.ok_or(HostError::Corrupt(
                    "unique request_id conflict but row missing",
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// 按内容保存原文。
    ///
    /// **作用**：返回 `sha256:` 引用。相同字节返回同一引用。
    /// **边界**：`media_type` 必须非空，但不写入文件或数据库。
    pub fn put_blob(&self, bytes: &[u8], media_type: &str) -> Result<BlobRef, HostError> {
        blob::put(&self.shared.root, bytes, media_type)
    }

    /// 按引用读回原文。
    ///
    /// **作用**：把库存的 `sha256:` 引用还原成字节。
    /// **边界**：引用格式不对返回 `Invalid`。文件不存在返回 `NotFound`。不检查这些字节属于哪一次 Run。
    /// P0 不重新计算哈希，文件被替换后读回的内容可以和引用不一致。
    pub fn read_blob(&self, blob_ref: &BlobRef) -> Result<Vec<u8>, HostError> {
        blob::read(&self.shared.root, blob_ref)
    }

    /// 补读已提交的 durable event。
    ///
    /// **作用**：返回该 Run 上 `sequence > after` 的事件，按序号升序。
    /// **边界**：只看见已 COMMIT 的行。不跨 Run 排序。
    pub fn events_after(&self, run_id: &RunId, after: u64) -> Result<Vec<Event>, HostError> {
        let connection = self.lock()?;
        events::load_after(&connection, run_id, after)
    }

    /// 列出该 Run 已持久化的 Artifact。
    ///
    /// **作用**：供 CLI `artifacts` 打印类型、摘要与 blob 引用。
    /// **边界**：按创建时间排序。不打开 blob 文件；正文用 `read_blob` 另取。
    pub fn list_artifacts(&self, run_id: &RunId) -> Result<Vec<Artifact>, HostError> {
        let connection = self.lock()?;
        store::load_run(&connection, run_id)?;
        events::list_artifacts(&connection, run_id)
    }

    /// 从权威表组装快照。
    ///
    /// **作用**：在一条 `DEFERRED` 读事务里带上当前状态、`MAX(sequence)` 和 Artifact 列表。
    /// **边界**：不另建快照表。没有 event 时 `last_sequence` 为 0。不把全部 event 反序列化出来。
    pub fn snapshot(&self, run_id: &RunId) -> Result<RunSnapshot, HostError> {
        let mut connection = self.lock()?;
        let transaction = store::deferred(&mut connection)?;
        let run = store::load_run(&transaction, run_id)?;
        let last_sequence = events::max_sequence(&transaction, run_id)?;
        let artifact_ids = events::artifact_ids(&transaction, run_id)?;
        transaction.commit()?;
        let snapshot = RunSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            run_id: run.run_id.clone(),
            session_id: run.session_id,
            status: run.status,
            terminal_reason: run.terminal_reason,
            last_sequence,
            input_ref: run.input_ref,
            workspace_ref: run.workspace_ref,
            budget: run.budget,
            artifact_ids,
            updated_at: run.updated_at,
        };
        if snapshot.invariants_hold() {
            Ok(snapshot)
        } else {
            Err(HostError::Corrupt("snapshot"))
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
    /// **作用**：启动前提交 `run.started` 与 `attempt.started`，结束后把报告、Attempt 和 Run 终态放进同一事务。
    /// **参数**：`engine` 由 CLI 组装；其 `descriptor` 须与 Attempt 已存身份一致（本阶段 create 时写入）。
    /// **边界**：不持锁越过 `.await`。报告正文先进 blob，事务只写引用和 event。
    /// `prepare_drive` 在进入 `Running` 之前失败时，按 ADR-021 先写成 `Failed(engine_error)`，再返回
    /// `Ok(EngineOutcome::Failed { .. })`，不调用 Engine。
    /// 取消令牌在 `prepare_drive` **之前**登记，所以 `Running` 提交之后的取消不会落在登记窗口里；同一进程内同一 Run 已有
    /// drive 时返回 `Invalid`。租约每 [`LEASE_RENEW_INTERVAL_SECS`] 续约一次，**直到 `engine.run` 返回**才停，
    /// 取消触发不会停掉续期（Engine 收尾可能很慢）。续期任务与令牌登记由 [`DriveGuard`] 托底：panic 或 future 被丢弃都会清理。
    /// 若收尾时 Run 已是 `Cancelling`，无论 Engine 交回什么都写成 `Cancelled`（先提交的取消意图优先）。
    /// 若收尾时已是终态（例如扫描写成 `host_restart`），不再用 Engine 结果改写。
    pub async fn drive_run(
        &self,
        run_id: &RunId,
        engine: &dyn EngineAdapter,
    ) -> Result<EngineOutcome, HostError> {
        let cancel = CancellationToken::new();
        self.register_cancel(run_id, cancel.clone())?;
        let mut guard = DriveGuard::new(self.clone(), run_id.clone());
        let prepared = match self.prepare_drive(run_id) {
            Ok(prepared) => prepared,
            Err(PrepareDriveError::UnstartedFailed) => {
                return Ok(EngineOutcome::Failed {
                    reason: FailureReason::EngineError,
                    partial: None,
                });
            }
            Err(PrepareDriveError::Host(error)) => return Err(error),
        };
        guard.start_renewal();
        let host: Arc<dyn HostPort> = Arc::new(DrivePort::new(
            Arc::clone(&self.shared),
            run_id.clone(),
            prepared.budget,
            cancel.clone(),
        ));
        let outcome = engine.run(prepared.request, host).await;
        drop(guard);
        self.finish_drive(run_id, outcome)
    }

    /// 在调用 Engine 之前把 Run 标成运行中。
    ///
    /// **作用**：同一事务写入 `Running`、`run.started` 和 `attempt.started`，并把任务原文与已打开的授权根交给 Engine。
    /// **边界**：任务原文与工作区打开都在写锁外完成。能否从 `Accepted` 进入 `Running` 在 `IMMEDIATE` 事务内判断，避免两个进程同时启动。
    /// 任务 blob 缺失 / 非 UTF-8、工作区打不开或规范化后与库存不一致时：调用 [`abort_unstarted`]，返回 `UnstartedFailed`（见 ADR-021）。
    /// 「不能启动」（已非 Accepted / Starting）返回 `Host(Invalid)`，不改状态。测试可在进入 Running 的 COMMIT 前中止。
    fn prepare_drive(&self, run_id: &RunId) -> Result<PreparedDrive, PrepareDriveError> {
        let (input_ref, workspace_ref) = {
            let connection = self.lock().map_err(PrepareDriveError::Host)?;
            let run = store::load_run(&connection, run_id).map_err(PrepareDriveError::Host)?;
            let attempt =
                store::latest_attempt(&connection, run_id).map_err(PrepareDriveError::Host)?;
            // 先看状态再碰 blob / 工作区：非 Accepted 的 Run 不归本次 drive，不能因为 IO 失败被改写。
            if run.status != RunStatus::Accepted || attempt.status != AttemptStatus::Starting {
                return Err(PrepareDriveError::Host(HostError::Invalid(
                    "run is not startable",
                )));
            }
            (run.input_ref, run.workspace_ref)
        };
        let input = match read_task_text(&self.shared.root, &input_ref) {
            Ok(text) => text,
            Err(_) => {
                self.abort_unstarted(run_id, "input_blob")
                    .map_err(PrepareDriveError::Host)?;
                return Err(PrepareDriveError::UnstartedFailed);
            }
        };
        let workspace = match WorkspaceRoot::open(&workspace_ref) {
            Ok(root) => root,
            Err(_) => {
                self.abort_unstarted(run_id, "workspace root is unavailable")
                    .map_err(PrepareDriveError::Host)?;
                return Err(PrepareDriveError::UnstartedFailed);
            }
        };
        // 库存的是创建时规范化的路径。重新规范化后不同，说明路径中间被换成了符号链接或挂载点。
        if workspace.as_path().to_str() != Some(workspace_ref.as_str()) {
            self.abort_unstarted(run_id, "workspace root changed since creation")
                .map_err(PrepareDriveError::Host)?;
            return Err(PrepareDriveError::UnstartedFailed);
        }
        let fail = self.take_fail_prepare();
        let mut connection = self.lock().map_err(PrepareDriveError::Host)?;
        let transaction = store::immediate(&mut connection).map_err(PrepareDriveError::Host)?;
        let mut run = store::load_run(&transaction, run_id).map_err(PrepareDriveError::Host)?;
        let mut attempt =
            store::latest_attempt(&transaction, run_id).map_err(PrepareDriveError::Host)?;
        if !run_transition_allowed(run.status, RunStatus::Running)
            || !attempt_transition_allowed(attempt.status, AttemptStatus::Running)
        {
            return Err(PrepareDriveError::Host(HostError::Invalid(
                "run is not startable",
            )));
        }
        run.status = RunStatus::Running;
        run.updated_at = now_utc();
        attempt.status = AttemptStatus::Running;
        store::update_run_and_attempt(&transaction, &run, &attempt)
            .map_err(PrepareDriveError::Host)?;
        let mut sequence =
            events::max_sequence(&transaction, run_id).map_err(PrepareDriveError::Host)? + 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::started(),
        )
        .map_err(PrepareDriveError::Host)?;
        sequence += 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            attempt.attempt_id.as_str(),
            events::attempt_started(
                attempt.attempt_id.clone(),
                attempt.owner.clone(),
                attempt.lease_expires_at.clone(),
            ),
        )
        .map_err(PrepareDriveError::Host)?;
        let budget = run.budget;
        commit_tx(transaction, fail).map_err(PrepareDriveError::Host)?;
        Ok(PreparedDrive {
            request: EngineRunRequest {
                run_id: run.run_id,
                input,
                workspace,
            },
            budget,
        })
    }

    /// 从未进入 `Running` 时把 Run / Attempt 收成失败终态。
    ///
    /// **作用**：§4.7.4 / ADR-021：任务或工作区在 drive 前不可用时，同事务写 `attempt.ended` 与 `run.failed`。
    /// **参数**：`error_reason` 写入 Attempt，须非空；也用作日志线索，不进 event payload 的敏感字段。
    /// **边界**：仅当事务内仍是 `Accepted` + `Starting` 时改写；否则返回 `Invalid("run is not startable")`，不改任何行。
    /// 这样别的进程已把 Run 推进到 `Running` 或终态时，本路径不会覆盖它。不写 Artifact。
    fn abort_unstarted(&self, run_id: &RunId, error_reason: &str) -> Result<(), HostError> {
        if error_reason.is_empty() {
            return Err(HostError::Invalid("error_reason must not be empty"));
        }
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let mut run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        if run.status != RunStatus::Accepted || attempt.status != AttemptStatus::Starting {
            return Err(HostError::Invalid("run is not startable"));
        }
        run.status = RunStatus::Failed;
        run.terminal_reason = Some(TerminalReason::EngineError);
        run.updated_at = now_utc();
        attempt.status = AttemptStatus::Ended;
        attempt.outcome = Some(AttemptOutcome::Failed);
        attempt.error_reason = Some(error_reason.to_owned());
        if !run.invariants_hold() || !attempt.invariants_hold() {
            return Err(HostError::Invalid("abort records violate invariants"));
        }
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        let mut sequence = events::max_sequence(&transaction, run_id)? + 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            attempt.attempt_id.as_str(),
            events::attempt_ended(attempt.attempt_id.clone(), AttemptOutcome::Failed),
        )?;
        sequence += 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::terminal(TerminalReason::EngineError),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 按 `EngineOutcome` 写入终态。
    ///
    /// **作用**：有报告时先把摘要正文写成 blob，再在同一事务写入 Artifact、`artifact.created`、`attempt.ended` 和 Run 终态事件。
    /// **参数**：`outcome` 是 Engine 交回值；若库中已是 `Cancelling`，会先经 [`force_cancelled`] 再落库。
    /// **边界**：状态是否还能收尾，在 `IMMEDIATE` 事务内判断。`artifact_type` 为空时改记 `Failed(engine_error)`。
    /// 没有报告则不写 Artifact。blob 在写锁外落盘，回滚后允许留下孤儿文件。
    /// 若事务内已是终态（例如扫描写成 `host_restart`），不再用 Engine 结果改写，直接返回传入的 `outcome`。
    fn finish_drive(
        &self,
        run_id: &RunId,
        outcome: EngineOutcome,
    ) -> Result<EngineOutcome, HostError> {
        let mut outcome = normalize_outcome(outcome);
        // `force_cancelled` 只把 Completed 的报告降为 partial，草稿内容不变，所以 blob 可以先写；
        // 是否 Cancelling 只在事务内判断一次。
        let draft = report_draft(&outcome).cloned();
        let stored = draft
            .as_ref()
            .map(|draft| blob::put(&self.shared.root, draft.summary.as_bytes(), "text/plain"))
            .transpose()?;
        let fail = self.take_fail_finish();
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let mut run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        // 扫描已把 Run 收成终态（如 host_restart）时，不再用 Engine 结果改写。
        if matches!(
            run.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            transaction.commit()?;
            return Ok(outcome);
        }
        if run.status == RunStatus::Cancelling {
            outcome = force_cancelled(outcome);
        }
        apply_outcome(&mut run, &mut attempt, &outcome)?;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        let mut sequence = events::max_sequence(&transaction, run_id)? + 1;
        if let Some(draft) = draft {
            let artifact = Artifact {
                artifact_id: ArtifactId::generate(),
                run_id: run.run_id.clone(),
                artifact_type: draft.artifact_type.clone(),
                storage_ref: stored
                    .as_ref()
                    .expect("draft blob is stored before the transaction")
                    .as_str()
                    .to_owned(),
                summary: draft.summary.clone(),
                created_at: now_utc(),
            };
            events::insert_artifact(&transaction, &artifact)?;
            events::record(
                &transaction,
                run_id,
                sequence,
                artifact.artifact_id.as_str(),
                events::artifact_created(&artifact),
            )?;
            sequence += 1;
        }
        let outcome_value = attempt.outcome.expect("ended attempt has an outcome");
        events::record(
            &transaction,
            run_id,
            sequence,
            attempt.attempt_id.as_str(),
            events::attempt_ended(attempt.attempt_id.clone(), outcome_value),
        )?;
        sequence += 1;
        let reason = run.terminal_reason.expect("terminal run has a reason");
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::terminal(reason),
        )?;
        commit_tx(transaction, fail)?;
        Ok(outcome)
    }

    /// 登记本进程 drive 的取消令牌。
    ///
    /// **作用**：让同进程上的 [`Self::request_cancel`] / [`Self::orphan_drive`] 能叫醒 Engine。
    /// **参数**：`token` 与交给 `DrivePort` 的是同一把。
    /// **边界**：同 `run_id` 已有登记（本进程另一个 drive 在跑）返回 `Invalid`，不覆盖。锁毒化返回 `Poisoned`。
    fn register_cancel(&self, run_id: &RunId, token: CancellationToken) -> Result<(), HostError> {
        let mut map = self
            .shared
            .active_cancels
            .lock()
            .map_err(|_| HostError::Poisoned)?;
        match map.entry(run_id.as_str().to_owned()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(HostError::Invalid("run is already being driven"))
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(token);
                Ok(())
            }
        }
    }

    /// 去掉登记，避免误触已结束的 drive。
    ///
    /// **作用**：[`DriveGuard`] 析构时调用，防止迟到的 `request_cancel` 点到已复用的令牌槽。
    /// **边界**：锁毒化或键不存在时无操作。
    fn unregister_cancel(&self, run_id: &RunId) {
        if let Ok(mut map) = self.shared.active_cancels.lock() {
            map.remove(run_id.as_str());
        }
    }

    /// 触发已登记的取消令牌。
    ///
    /// **作用**：打断本进程正在 `await` 的 Engine。由 `request_cancel` 与 `orphan_drive` 调用。
    /// **边界**：本 `RunHost` 未登记该 `run_id` 时无操作并返回 `false`（跨进程只改库，令牌在原进程的 map 里）。
    fn trigger_cancel_token(&self, run_id: &RunId) -> bool {
        let Ok(map) = self.shared.active_cancels.lock() else {
            return false;
        };
        match map.get(run_id.as_str()) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// 把最新 Attempt 的租约延长到 `now + TTL`。
    ///
    /// **作用**：`drive_run` 内定时任务每 [`LEASE_RENEW_INTERVAL_SECS`] 调用，防止扫描误杀仍在跑的 Attempt。
    /// **边界**：仅当 Run 为 `Running` 或 `Cancelling` 且 Attempt 为 `Running` 时续期。其它状态返回 `Invalid`，调用方应停止续期循环。
    fn renew_lease(&self, run_id: &RunId) -> Result<(), HostError> {
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        if !matches!(run.status, RunStatus::Running | RunStatus::Cancelling)
            || attempt.status != AttemptStatus::Running
        {
            return Err(HostError::Invalid("run is not renewable"));
        }
        attempt.lease_expires_at = lease_expires_at();
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        transaction.commit()?;
        Ok(())
    }

    /// 租约过期：Attempt `Orphaned`，Run `Failed(host_restart)`。
    ///
    /// **作用**：由 [`Self::reap_expired_leases`] 调用；同事务写 `attempt.orphaned` 与 `run.failed`。
    /// **参数**：`run_id` 应对应仍非终态、Attempt 仍为 `Running` 的 drive。
    /// **边界**：返回 `Ok(true)` 表示本次确实收了尸。候选列表是事务外的旧快照，所以在 `IMMEDIATE` 事务里重新读 Attempt：
    /// 已终态、Attempt 不再是 `Running`、或租约已被续期（不再过期）都返回 `Ok(false)` 且不改库，避免误杀刚续期的活进程。
    /// 成功后触发本进程取消令牌，避免仍在 await 的 `drive_run` 挂死。
    fn orphan_drive(&self, run_id: &RunId) -> Result<bool, HostError> {
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let mut run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        if matches!(
            run.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) || attempt.status != AttemptStatus::Running
            || !lease_is_expired(&attempt.lease_expires_at, &now_utc())
        {
            return Ok(false);
        }
        if !run_transition_allowed(run.status, RunStatus::Failed)
            || !attempt_transition_allowed(attempt.status, AttemptStatus::Orphaned)
        {
            return Err(HostError::Invalid("run is not orphanable"));
        }
        run.status = RunStatus::Failed;
        run.terminal_reason = Some(TerminalReason::HostRestart);
        run.updated_at = now_utc();
        attempt.status = AttemptStatus::Orphaned;
        attempt.outcome = None;
        attempt.error_reason = Some("host_restart".to_owned());
        if !run.invariants_hold() || !attempt.invariants_hold() {
            return Err(HostError::Invalid("orphan records violate invariants"));
        }
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        let mut sequence = events::max_sequence(&transaction, run_id)? + 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            attempt.attempt_id.as_str(),
            events::attempt_orphaned(attempt.attempt_id.clone()),
        )?;
        sequence += 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::terminal(TerminalReason::HostRestart),
        )?;
        transaction.commit()?;
        // 本进程若仍在 drive，解开 Engine 的等待，避免挂死。
        self.trigger_cancel_token(run_id);
        Ok(true)
    }

    /// 取出并清除「下次 prepare 在 COMMIT 前失败」测试开关。
    ///
    /// **作用**：仅测试构建有效；生产恒为 `false`。
    /// **边界**：一次消费；并发调用不保证顺序。
    fn take_fail_prepare(&self) -> bool {
        #[cfg(test)]
        {
            self.shared.fail_prepare.swap(false, Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// 取出并清除「下次 finish 在 COMMIT 前失败」测试开关。
    ///
    /// **作用**：仅测试构建有效；生产恒为 `false`。
    /// **边界**：一次消费；并发调用不保证顺序。
    fn take_fail_finish(&self) -> bool {
        #[cfg(test)]
        {
            self.shared.fail_finish.swap(false, Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// 取得本进程 SQLite 连接的互斥锁。
    ///
    /// **作用**：所有同步读库/写库入口共用这一把锁。
    /// **边界**：锁毒化返回 `Poisoned`。不跨 `.await` 持有。
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, HostError> {
        self.shared.conn.lock().map_err(|_| HostError::Poisoned)
    }
}

/// 提交一条已经写好状态和 event 的事务。
///
/// **作用**：让状态行和 durable event 同时可见。
/// **边界**：测试可以把 `fail` 设为真，在 COMMIT 前回滚并返回 `Aborted`。生产构建忽略该参数，直接提交。
fn commit_tx(transaction: rusqlite::Transaction<'_>, fail: bool) -> Result<(), HostError> {
    #[cfg(test)]
    if fail {
        transaction.rollback()?;
        return Err(HostError::Aborted);
    }
    #[cfg(not(test))]
    let _ = fail;
    transaction.commit()?;
    Ok(())
}

/// 从 blob 引用读回任务文本。
///
/// **作用**：库存的是 `sha256:` 引用，Engine 收到的是原文。
/// **边界**：不持有数据库锁。文件不存在或内容不是 UTF-8 时视为库损坏。
fn read_task_text(root: &Path, input_ref: &str) -> Result<String, HostError> {
    let bytes = blob::read(root, &BlobRef::new(input_ref)).map_err(|error| match error {
        HostError::NotFound => HostError::Corrupt("input blob"),
        other => other,
    })?;
    String::from_utf8(bytes).map_err(|_| HostError::Corrupt("input blob"))
}

/// 一次 `drive_run` 的清理托底。
///
/// **作用**：持有租约续期任务，并在析构时停掉续期、撤销取消令牌登记。正常结束、`prepare_drive` 失败、Engine panic、
/// `drive_run` 的 future 被丢弃都走同一条清理路径，不会留下继续续期的游离任务。
/// **边界**：续期用自己的停止令牌，**不**跟 Engine 的取消令牌联动：取消只是让 Engine 收尾，Run 仍需要租约直到 `engine.run` 返回。
struct DriveGuard {
    host: RunHost,
    run_id: RunId,
    renew_stop: CancellationToken,
    renew_task: Option<tokio::task::JoinHandle<()>>,
}

impl DriveGuard {
    /// **作用**：登记完取消令牌后立刻建 guard，保证之后任何早退都会撤销登记。
    fn new(host: RunHost, run_id: RunId) -> Self {
        Self {
            host,
            run_id,
            renew_stop: CancellationToken::new(),
            renew_task: None,
        }
    }

    /// 开始每 [`LEASE_RENEW_INTERVAL_SECS`] 续期一次。
    ///
    /// **边界**：只在 Run 进入 `Running` 之后调用。数据库调用放进 `spawn_blocking`，不占住异步 worker。
    /// 续期报 `Invalid`（Run 已终态或被收尸）才停；其它错误（如 SQLite busy）记日志，下个周期再试。
    fn start_renewal(&mut self) {
        let host = self.host.clone();
        let run_id = self.run_id.clone();
        let stop = self.renew_stop.clone();
        self.renew_task = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(LEASE_RENEW_INTERVAL_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // `interval` 的第一次 tick 立即触发；租约刚写过，跳过它。
            ticker.tick().await;
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => break,
                    _ = ticker.tick() => {
                        let (host, run_id) = (host.clone(), run_id.clone());
                        match tokio::task::spawn_blocking(move || host.renew_lease(&run_id)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(HostError::Invalid(reason))) => {
                                log::error!("lease renewal stopped: {reason}");
                                break;
                            }
                            Ok(Err(error)) => log::error!("lease renewal failed: {error}"),
                            Err(error) => {
                                log::error!("lease renewal task failed: {error}");
                                break;
                            }
                        }
                    }
                }
            }
        }));
    }
}

impl Drop for DriveGuard {
    fn drop(&mut self) {
        self.renew_stop.cancel();
        if let Some(task) = self.renew_task.take() {
            task.abort();
        }
        self.host.unregister_cancel(&self.run_id);
    }
}

/// `prepare_drive` 交给 `drive_run` 的任务与预算上限。
///
/// **作用**：一次打开工作区并进入 `Running` 后，把 `EngineRunRequest` 与本次 drive 的预算上限一起交出。
/// **边界**：`budget` 只用于构造 `DrivePort` 的内存计数器，不表示已用量。
struct PreparedDrive {
    request: EngineRunRequest,
    budget: Budget,
}

/// `prepare_drive` 的内部失败分类。
///
/// **作用**：把「已写成 Failed 终态」与「普通 Host 错误」分开，供 `drive_run` 映射返回值。
enum PrepareDriveError {
    /// 已调用 `abort_unstarted`，Run 为 `Failed(engine_error)`。
    UnstartedFailed,
    Host(HostError),
}

/// 空的产物类型不能收成成功。
///
/// **作用**：把这种 Engine 返回改写成 `Failed(engine_error)`，让 Run 离开 `Running`。
/// **参数**：`outcome` 是 Engine 交回值；仅当草稿 `artifact_type` 为空时改写。
/// **边界**：不保留那份空类型草稿，因此也不会写 Artifact。其它失败/取消原样返回。
fn normalize_outcome(outcome: EngineOutcome) -> EngineOutcome {
    if report_draft(&outcome).is_some_and(|draft| draft.artifact_type.is_empty()) {
        EngineOutcome::Failed {
            reason: FailureReason::EngineError,
            partial: None,
        }
    } else {
        outcome
    }
}

/// 取消已受理时，把任意 Engine 结果收成 `Cancelled`。
///
/// **作用**：先提交的 `Cancelling` 优先于随后到达的 Completed/Failed；由 [`RunHost::finish_drive`] 调用。
/// **参数**：`outcome` 是 Engine 原始或已经 `normalize_outcome` 过的值。
/// **边界**：`Completed` 的报告降为 partial；已有 partial 保留。不发明新的失败原因。
fn force_cancelled(outcome: EngineOutcome) -> EngineOutcome {
    let partial = match outcome {
        EngineOutcome::Completed { report } => Some(report),
        EngineOutcome::Failed { partial, .. } | EngineOutcome::Cancelled { partial } => partial,
    };
    EngineOutcome::Cancelled { partial }
}

/// 本进程还没有日志后端时，把 `fluxaxiom_run_host` 的错误写到 stderr。
struct HostLogger;

impl log::Log for HostLogger {
    /// **作用**：只接受本 crate 的 Error 级记录。
    /// **边界**：不放开其它 target，避免抢走应用已安装的全局后端。
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Error && metadata.target().starts_with("fluxaxiom_run_host")
    }

    /// **作用**：把已启用的记录打到 stderr，前缀 `fluxaxiom-run-host:`。
    /// **边界**：不打印密钥或 blob 路径以外的业务正文；本实现只转发 `record.args()`。
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("fluxaxiom-run-host: {}", record.args());
        }
    }

    /// **作用**：满足 `log::Log`；stderr 无需额外 flush。
    fn flush(&self) {}
}

/// 进程还没有日志后端时，把本 crate 的错误打到 stderr。
///
/// **作用**：`Storage` 不携带原因，原因必须有地方可见。
/// **边界**：已经安装过后端时不再替换，也不改全局日志级别。CLI 若要自己接管，需在 `RunHost::open` 之前安装。
fn install_host_log() {
    static LOGGER: HostLogger = HostLogger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Error);
    }
}

/// 从 Engine 的返回值取出要落盘的报告草稿。
///
/// **作用**：`Completed` 使用正式报告，失败或取消使用部分报告。
/// **边界**：没有部分报告时返回空，调用方不写 Artifact。
fn report_draft(outcome: &EngineOutcome) -> Option<&ArtifactDraft> {
    match outcome {
        EngineOutcome::Completed { report } => Some(report),
        EngineOutcome::Failed { partial, .. } | EngineOutcome::Cancelled { partial } => {
            partial.as_ref()
        }
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

/// 把调用方给的工作区路径收成库存用的规范化绝对路径。
///
/// **作用**：`create_run` 在写任何东西之前调用，让授权在创建时一次确定。
/// **参数**：`raw` 是调用方给的目录，可以是相对路径，按当前进程的当前目录解析这一次。
/// **边界**：目录不存在、不是目录、无法规范化返回 `Invalid("workspace root is unavailable")`。规范化结果不是 UTF-8 返回 `Invalid("workspace path is not utf-8")`，因为库存是字符串，用有损转换会让后面的相等比较失效。
fn canonical_workspace_ref(raw: &str) -> Result<String, HostError> {
    let root = WorkspaceRoot::open(raw)
        .map_err(|_| HostError::Invalid("workspace root is unavailable"))?;
    root.as_path()
        .to_str()
        .map(str::to_owned)
        .ok_or(HostError::Invalid("workspace path is not utf-8"))
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
    /// 读当前 `journal_mode` pragma，供打开库后的断言。
    pub(crate) fn journal_mode(&self) -> Result<String, HostError> {
        let connection = self.lock()?;
        store::journal_mode(&connection)
    }

    /// 读当前 `busy_timeout`（毫秒），供打开库后的断言。
    pub(crate) fn busy_timeout_ms(&self) -> Result<i64, HostError> {
        let connection = self.lock()?;
        store::busy_timeout_ms(&connection)
    }
}

#[cfg(test)]
impl RunHost {
    /// 下次 `prepare_drive` 的 COMMIT 前回滚并返回 `Aborted`。
    ///
    /// **作用**：单测原子性；一次生效，经 [`Self::take_fail_prepare`] 消费。
    fn fail_next_prepare_commit(&self) {
        self.shared.fail_prepare.store(true, Ordering::SeqCst);
    }

    /// 下次 `finish_drive` 的 COMMIT 前回滚并返回 `Aborted`。
    ///
    /// **作用**：单测原子性；一次生效，经 [`Self::take_fail_finish`] 消费。
    fn fail_next_finish_commit(&self) {
        self.shared.fail_finish.store(true, Ordering::SeqCst);
    }

    /// 测试用：在不经过 `prepare_drive` 时构造端口，用来验证非 `Running` 的 `reserve`。
    ///
    /// **参数**：`budget` 写入内存计数上限；取消令牌是孤立的新令牌，不登记进 `active_cancels`。
    /// **边界**：不改库状态；调用方须自行保证 `run_id` 已存在。
    pub(crate) fn drive_port_for_test(&self, run_id: &RunId, budget: Budget) -> DrivePort {
        DrivePort::new(
            Arc::clone(&self.shared),
            run_id.clone(),
            budget,
            CancellationToken::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use fluxaxiom_contract::{EventBody, RunStatus};
    use fluxaxiom_engine_api::{
        BudgetExhausted, BudgetRequest, EngineAdapter, EngineDescriptor, EngineOutcome,
        EngineRunRequest, HostPort,
    };
    use std::sync::Arc;

    use super::RunHost;
    use crate::error::HostError;

    #[test]
    fn opens_with_wal_and_five_second_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let host = RunHost::open(directory.path()).unwrap();
        assert!(host.journal_mode().unwrap().eq_ignore_ascii_case("wal"));
        assert_eq!(host.busy_timeout_ms().unwrap(), 5_000);
    }

    /// COMMIT 前失败时，Running 和 `run.started` 都不能留下。
    #[tokio::test]
    async fn prepare_commit_failure_rolls_back_status_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host.create_run(sample_request("req_abort", path)).unwrap();
        host.fail_next_prepare_commit();
        let error = host
            .drive_run(&created.run.run_id, &PanicEngine)
            .await
            .unwrap_err();
        assert!(matches!(error, HostError::Aborted));
        assert_eq!(
            host.get_run(&created.run.run_id).unwrap().status,
            RunStatus::Accepted
        );
        let events = host.events_after(&created.run.run_id, 0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);
        assert!(matches!(events[0].body, EventBody::RunAccepted(_)));
    }

    /// 收尾事务失败时，Run 仍停在 Running，终态 event 不出现。
    #[tokio::test]
    async fn finish_commit_failure_rolls_back_terminal_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(sample_request("req_finish_abort", path))
            .unwrap();
        host.fail_next_finish_commit();
        let error = host
            .drive_run(&created.run.run_id, &CompleteEngine)
            .await
            .unwrap_err();
        assert!(matches!(error, HostError::Aborted));
        assert_eq!(
            host.get_run(&created.run.run_id).unwrap().status,
            RunStatus::Running
        );
        let events = host.events_after(&created.run.run_id, 0).unwrap();
        assert!(events
            .iter()
            .all(|event| !matches!(event.body, EventBody::RunCompleted(_))));
    }

    /// Accepted 的 Run 上 `reserve` 返回 `BudgetExhausted`，不发 lease。
    ///
    /// **边界**：包括 `{0,0}`。错误类型不区分「没额度」和「未在 Running」。
    #[test]
    fn reserve_before_running_is_exhausted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(sample_request("req_reserve_early", path))
            .unwrap();
        let port = host.drive_port_for_test(&created.run.run_id, created.run.budget);
        assert_eq!(
            port.reserve(BudgetRequest {
                model_calls: 0,
                tool_calls: 0,
            }),
            Err(BudgetExhausted)
        );
        assert_eq!(
            port.reserve(BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            }),
            Err(BudgetExhausted)
        );
        assert_eq!(
            host.get_run(&created.run.run_id).unwrap().status,
            RunStatus::Accepted
        );
    }

    /// Completed 之后再拿端口 `reserve`，同样是 `BudgetExhausted`。
    #[tokio::test]
    async fn reserve_after_completed_is_exhausted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(sample_request("req_reserve_done", path))
            .unwrap();
        host.drive_run(&created.run.run_id, &CompleteEngine)
            .await
            .unwrap();
        let port = host.drive_port_for_test(&created.run.run_id, created.run.budget);
        assert_eq!(
            port.reserve(BudgetRequest {
                model_calls: 1,
                tool_calls: 0,
            }),
            Err(BudgetExhausted)
        );
        assert_eq!(
            host.get_run(&created.run.run_id).unwrap().status,
            RunStatus::Completed
        );
    }

    /// Accepted 的 Run 还不能接收观测。
    #[test]
    fn observe_before_start_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(sample_request("req_observe_early", path))
            .unwrap();
        let reference = host.put_blob(b"body", "text/plain").unwrap();
        let error = crate::port::observe_durable(
            host.shared.as_ref(),
            &created.run.run_id,
            &fluxaxiom_engine_api::EngineObservation::ToolCall {
                tool_name: "read_file".to_owned(),
                normalized_path_or_query: "src/lib.rs".to_owned(),
                result_ref: reference,
                byte_count: 4,
                truncated: false,
                duration_ms: 1,
            },
        )
        .unwrap_err();
        assert!(matches!(error, HostError::Invalid(_)));
        assert_eq!(
            host.get_run(&created.run.run_id).unwrap().status,
            RunStatus::Accepted
        );
    }

    /// 收尸前在事务内重查租约：候选快照过期、但已被续期的 Run 不能被收成孤儿。
    ///
    /// **边界**：直接调 `orphan_drive`，模拟「列候选时过期、拿到写锁前被续期」的旧快照。
    #[test]
    fn orphan_drive_skips_a_lease_renewed_after_the_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let host = RunHost::open(path).unwrap();
        let created = host
            .create_run(sample_request("req_orphan_recheck", path))
            .unwrap();
        let run_id = created.run.run_id.clone();
        assert!(host.prepare_drive(&run_id).is_ok());

        assert!(!host.orphan_drive(&run_id).unwrap());
        assert_eq!(host.get_run(&run_id).unwrap().status, RunStatus::Running);

        host.expire_lease_for_test(&run_id).unwrap();
        assert!(host.orphan_drive(&run_id).unwrap());
        assert_eq!(host.get_run(&run_id).unwrap().status, RunStatus::Failed);
        // 已终态：再收一次是空操作。
        assert!(!host.orphan_drive(&run_id).unwrap());
    }

    /// 测试用创建参数。`parent` 下会建可 `open` 的 `authorized/` 目录。
    fn sample_request(request_id: &str, parent: &std::path::Path) -> crate::host::CreateRun {
        let workspace = parent.join("authorized");
        std::fs::create_dir_all(&workspace).unwrap();
        crate::host::CreateRun {
            request_id: request_id.to_owned(),
            session_id: None,
            input_ref: "analyze runtime".to_owned(),
            workspace_ref: workspace.to_string_lossy().into_owned(),
            budget: fluxaxiom_contract::Budget {
                max_model_calls: 1,
                max_tool_calls: 1,
            },
            engine: EngineDescriptor {
                engine_id: "fake".to_owned(),
                version: "0.1.0".to_owned(),
            },
        }
    }

    struct CompleteEngine;

    #[async_trait]
    impl EngineAdapter for CompleteEngine {
        fn descriptor(&self) -> EngineDescriptor {
            EngineDescriptor {
                engine_id: "fake".to_owned(),
                version: "0.1.0".to_owned(),
            }
        }

        async fn run(&self, _request: EngineRunRequest, _host: Arc<dyn HostPort>) -> EngineOutcome {
            EngineOutcome::Completed {
                report: fluxaxiom_engine_api::ArtifactDraft {
                    artifact_type: "report".to_owned(),
                    summary: "done".to_owned(),
                },
            }
        }
    }

    struct PanicEngine;

    #[async_trait]
    impl EngineAdapter for PanicEngine {
        fn descriptor(&self) -> EngineDescriptor {
            EngineDescriptor {
                engine_id: "fake".to_owned(),
                version: "0.1.0".to_owned(),
            }
        }

        async fn run(&self, _request: EngineRunRequest, _host: Arc<dyn HostPort>) -> EngineOutcome {
            panic!("engine must not start when prepare commit is aborted");
        }
    }
}
