use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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

use crate::blob;
use crate::error::{is_constraint_violation, HostError};
use crate::events;
use crate::port::DrivePort;
use crate::store;
use crate::time::{lease_expires_at, now_utc};

/// 进程内共享的库连接和数据目录。
///
/// **作用**：`RunHost` 与 `DrivePort` 共用同一把锁和同一个 blob 根目录。
/// **边界**：锁只串行化本进程。跨进程仍靠 SQLite 的 WAL 和 `busy_timeout`。
pub(crate) struct Shared {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) root: PathBuf,
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

/// Session、Run、Attempt 和 durable event 的权威写入点。Engine 不持有这个类型。
///
/// **作用**：进程内用互斥锁串行化同一连接；跨进程靠 WAL 与 `busy_timeout` 等待写锁。
/// **边界**：写 event 的事务使用 `BEGIN IMMEDIATE`。blob 文件不在这些事务里。
pub struct RunHost {
    shared: Arc<Shared>,
}

impl RunHost {
    /// 打开或创建 Host 数据目录。
    ///
    /// **作用**：在目录里建 `host.db`，启用 WAL，跑完迁移，并准备 `blobs/`。
    /// **边界**：参数是目录，不是数据库文件。blob 放在 `blobs/ab/cd/<hex>`。
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, HostError> {
        install_host_log();
        let root = directory.as_ref();
        std::fs::create_dir_all(root.join("blobs").join("tmp"))?;
        let mut connection = store::connect(&root.join("host.db"))?;
        store::migrate(&mut connection)?;
        Ok(Self {
            shared: Arc::new(Shared {
                conn: Mutex::new(connection),
                root: root.to_path_buf(),
                #[cfg(test)]
                fail_prepare: AtomicBool::new(false),
                #[cfg(test)]
                fail_finish: AtomicBool::new(false),
            }),
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
    /// **边界**：不持锁越过 `.await`。报告正文先进 blob，事务只写引用和 event。
    /// **已知缺口（A3 前必须决定，见执行方案 §4.7.4）**：`prepare_drive` 失败时这里直接返回 `Err`，Run 仍是 `Accepted`，没有终态、没有事件。
    /// 本方法不会把它转成 `Failed`，也没有别的路径会回收它。A3 的 CLI 不能只打印这个错误就退出，否则用户之后看到的是一个永远不结束的 Run。
    pub async fn drive_run(
        &self,
        run_id: &RunId,
        engine: &dyn EngineAdapter,
    ) -> Result<EngineOutcome, HostError> {
        let request = self.prepare_drive(run_id)?;
        let host: Arc<dyn HostPort> =
            Arc::new(DrivePort::new(Arc::clone(&self.shared), run_id.clone()));
        let outcome = engine.run(request, host).await;
        self.finish_drive(run_id, outcome)
    }

    /// 在调用 Engine 之前把 Run 标成运行中。
    ///
    /// **作用**：同一事务写入 `Running`、`run.started` 和 `attempt.started`，并把任务原文与已打开的授权根交给 Engine。
    /// **边界**：任务原文与工作区打开都在写锁外完成。能否从 `Accepted` 进入 `Running` 在 `IMMEDIATE` 事务内判断，避免两个进程同时启动。任务 blob 缺失、不是 UTF-8、或工作区目录打不开时不改状态。测试可在 COMMIT 前中止。
    ///
    /// **失败后 Run 停在 `Accepted`（有意保留的缺口，不是遗漏）**：
    /// - 工作区在 `create_run` 已经校验并规范化过，所以现在这条路径剩下的是创建**之后**目录被删除、被替换成指向别处的符号链接等情况，不再包含「相对路径按另一个当前目录解析」。
    /// - 上面三种失败发生在事务开始之前，所以既不写 `run.failed`，也不改 Run 或 Attempt 状态。这是 A1.1 为了「失败不留半截状态」选的最保守做法，`missing_workspace_leaves_run_accepted` 测试固定了它。
    /// - 代价：这个 Run 不再是 `Running` / `Cancelling`，执行方案 §4.8 的启动扫描只处理这两种状态且租约已过期的 Attempt，所以它不会被标成 `Orphaned`，也不会变成 `Failed(host_restart)`。没有任何现有路径会收尾它。
    /// - 对用户可见的后果：`status` 永远是 `Accepted`，`events` 里没有任何事件解释原因。同一个 `request_id` 重新提交会按幂等返回这个旧 Run，而不是新建，用户无法靠重试摆脱。
    /// - A3 必须在这两种里选一个并写进 ADR：(1) 失败时同一事务写 `run.failed` 并终态化；(2) 保持 `Accepted` 但提供显式的放弃或重试命令。原先的第三种（创建前校验）已由 `create_run` 完成，它缩小了这条路径但不能消除它。不能默认留着。
    fn prepare_drive(&self, run_id: &RunId) -> Result<EngineRunRequest, HostError> {
        let (input_ref, workspace_ref) = {
            let connection = self.lock()?;
            let run = store::load_run(&connection, run_id)?;
            (run.input_ref, run.workspace_ref)
        };
        let input = read_task_text(&self.shared.root, &input_ref)?;
        // 失败时直接返回，Run 保持 `Accepted`。原因与后果见上面的函数注释。
        let workspace = WorkspaceRoot::open(&workspace_ref)
            .map_err(|_| HostError::Invalid("workspace root is unavailable"))?;
        // 库存的是创建时规范化的路径。重新规范化后不同，说明路径中间被换成了符号链接或挂载点，授权的不再是这个目录。
        if workspace.as_path().to_str() != Some(workspace_ref.as_str()) {
            return Err(HostError::Invalid("workspace root changed since creation"));
        }
        let fail = self.take_fail_prepare();
        let mut connection = self.lock()?;
        let transaction = store::immediate(&mut connection)?;
        let mut run = store::load_run(&transaction, run_id)?;
        let mut attempt = store::latest_attempt(&transaction, run_id)?;
        if !run_transition_allowed(run.status, RunStatus::Running)
            || !attempt_transition_allowed(attempt.status, AttemptStatus::Running)
        {
            return Err(HostError::Invalid("run is not startable"));
        }
        run.status = RunStatus::Running;
        run.updated_at = now_utc();
        attempt.status = AttemptStatus::Running;
        store::update_run_and_attempt(&transaction, &run, &attempt)?;
        let mut sequence = events::max_sequence(&transaction, run_id)? + 1;
        events::record(
            &transaction,
            run_id,
            sequence,
            run.run_id.as_str(),
            events::started(),
        )?;
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
        )?;
        commit_tx(transaction, fail)?;
        Ok(EngineRunRequest {
            run_id: run.run_id,
            input,
            workspace,
        })
    }

    /// 按 `EngineOutcome` 写入终态。
    ///
    /// **作用**：有报告时先把摘要正文写成 blob，再在同一事务写入 Artifact、`artifact.created`、`attempt.ended` 和 Run 终态事件。
    /// **边界**：状态是否还能收尾，在 `IMMEDIATE` 事务内判断。`artifact_type` 为空时改记 `Failed(engine_error)`，不把 Run 留在 `Running`。没有报告则不写 Artifact。blob 在写锁外落盘，回滚后允许留下孤儿文件。
    fn finish_drive(
        &self,
        run_id: &RunId,
        outcome: EngineOutcome,
    ) -> Result<EngineOutcome, HostError> {
        let outcome = normalize_outcome(outcome);
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

/// 空的产物类型不能收成成功。
///
/// **作用**：把这种 Engine 返回改写成 `Failed(engine_error)`，让 Run 离开 `Running`。
/// **边界**：不保留那份空类型草稿，因此也不会写 Artifact。
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

/// 本进程还没有日志后端时，把 `fluxaxiom_run_host` 的错误写到 stderr。
struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Error && metadata.target().starts_with("fluxaxiom_run_host")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("fluxaxiom-run-host: {}", record.args());
        }
    }

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
impl RunHost {
    fn fail_next_prepare_commit(&self) {
        self.shared.fail_prepare.store(true, Ordering::SeqCst);
    }

    fn fail_next_finish_commit(&self) {
        self.shared.fail_finish.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use fluxaxiom_contract::{EventBody, RunStatus};
    use fluxaxiom_engine_api::{
        EngineAdapter, EngineDescriptor, EngineOutcome, EngineRunRequest, HostPort,
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
