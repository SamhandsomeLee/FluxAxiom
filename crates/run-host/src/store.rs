//! Host 的 SQLite 行读写。
//!
//! **作用**：`RunHost` 与 `DrivePort` 经这些函数插入、读取和更新 Session、Run、Attempt、反馈。
//! **边界**：不写 durable event，不碰 blob 文件。状态是否允许迁移由调用方在事务里判断。
//! 未知枚举和非法主键解码为 `HostError::Corrupt`，不把缺字段编成默认状态。

use std::path::Path;
use std::time::Duration;

use fluxaxiom_contract::{
    Attempt, AttemptId, AttemptOutcome, AttemptStatus, Budget, Run, RunId, RunStatus, Session,
    SessionId, TerminalReason, Timestamp,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::HostError;
use crate::time::now_utc;

/// 打开 `host.db` 并套上 WAL 与写等待。
///
/// **作用**：`RunHost::open` 用它拿到进程内共享的那条连接。
/// **参数**：`path` 是数据目录下的库文件；父目录须已存在。
/// **边界**：文件打不开时返回 `Sqlite`。`journal_mode` 不是 WAL 时返回 `Corrupt`，不继续迁移。
pub(crate) fn connect(path: &Path) -> Result<Connection, HostError> {
    let connection = Connection::open(path)?;
    configure(&connection)?;
    Ok(connection)
}

/// 给一条已打开的连接设置 WAL、外键和写锁等待。
///
/// **作用**：`connect` 在返回连接前调用；测试也可直接打在内存库上。
/// **边界**：`journal_mode` 读回不是 `wal` 时返回 `Corrupt`，不把失败当成已启用 WAL。
/// 等待上限固定 5 秒。不建表、不迁移。
pub(crate) fn configure(connection: &Connection) -> Result<(), HostError> {
    // journal_mode 会返回当前模式，不能用只执行语句的 pragma_update。
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(HostError::Corrupt("journal_mode is not WAL"));
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(Duration::from_millis(5_000))?;
    Ok(())
}

/// 打开一条会写 event 的事务。
///
/// **作用**：`BEGIN IMMEDIATE`，先拿 reserved lock，再让调用方读状态和 `MAX(sequence)`。
/// **边界**：只包 SQLite 里的行。blob 文件的写入发生在事务外面。状态迁移是否允许必须在这条事务里判断。
pub(crate) fn immediate(connection: &mut Connection) -> Result<Transaction<'_>, HostError> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// 打开一条一致的只读事务。
///
/// **作用**：让快照里的状态和 `MAX(sequence)` 来自同一时刻。
/// **边界**：`DEFERRED`，不拿 reserved lock，不用于分配序号或改状态。
pub(crate) fn deferred(connection: &mut Connection) -> Result<Transaction<'_>, HostError> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Deferred)?)
}

/// 按版本顺序应用尚未记录的迁移。
///
/// **作用**：`RunHost::open` 在 `connect` 之后调用。版本记在 `schema_migrations`。
/// **边界**：已记录的版本不重跑，历史行不原地改写。每一版各自提交；中途失败时该版回滚，已提交的更低版本保留。
/// 不补写 durable event，也不校验业务行是否满足不变量。
pub(crate) fn migrate(connection: &mut Connection) -> Result<(), HostError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;
    let current: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    const MIGRATIONS: &[&str] = &[
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_events.sql"),
        include_str!("../migrations/0003_sample_feedback.sql"),
    ];
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).expect("migration count fits i64");
        if version <= current {
            continue;
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_utc().as_str()],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

/// 插入一行 Session。
///
/// **作用**：`RunHost::create_session` 在自己的事务里调用。Engine 不写这张表。
/// **参数**：`session` 的主键和 `created_at` 原样入库。
/// **边界**：主键冲突返回 `Sqlite` 约束错误。不创建 Run，不写 event。
pub(crate) fn insert_session(
    transaction: &Transaction<'_>,
    session: &Session,
) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO sessions (session_id, created_at) VALUES (?1, ?2)",
        params![session.session_id.as_str(), session.created_at.as_str()],
    )?;
    Ok(())
}

/// 插入一条新建 Run 和它的第一条 Attempt。
///
/// **作用**：`RunHost::create_run` 在 `BEGIN IMMEDIATE` 里调用；`run.accepted` 由调用方紧接着写入。
/// **参数**：`request_id` 是 Host 受理幂等键，不进入契约 `Run`。`run` / `attempt` 须已通过调用方的不变量检查。
/// **边界**：重复 `request_id` 返回 `Sqlite` 约束错误，由调用方回滚后再读已有行。不写 event，不校验状态机。
/// `session_id` 指向不存在的 Session 时同样是约束错误。
pub(crate) fn insert_created(
    transaction: &Transaction<'_>,
    run: &Run,
    attempt: &Attempt,
    request_id: &str,
) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO runs (
            run_id, request_id, session_id, status, terminal_reason,
            input_ref, workspace_ref, max_model_calls, max_tool_calls,
            created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.run_id.as_str(),
            request_id,
            run.session_id.as_ref().map(SessionId::as_str),
            encode_run_status(run.status),
            run.terminal_reason.map(encode_terminal_reason),
            run.input_ref,
            run.workspace_ref,
            run.budget.max_model_calls,
            run.budget.max_tool_calls,
            run.created_at.as_str(),
            run.updated_at.as_str(),
        ],
    )?;
    insert_attempt(transaction, attempt)?;
    Ok(())
}

/// 插入一行 Attempt。
///
/// **作用**：只被 `insert_created` 用来写下第一条 Attempt。
/// **边界**：`run_id` 必须已在本事务里存在，否则外键失败返回 `Sqlite`。不续租、不写 event。
fn insert_attempt(transaction: &Transaction<'_>, attempt: &Attempt) -> Result<(), HostError> {
    transaction.execute(
        "INSERT INTO attempts (
            attempt_id, run_id, engine_id, engine_version, owner,
            lease_expires_at, status, outcome, error_reason
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            attempt.attempt_id.as_str(),
            attempt.run_id.as_str(),
            attempt.engine_id,
            attempt.engine_version,
            attempt.owner,
            attempt.lease_expires_at.as_str(),
            encode_attempt_status(attempt.status),
            attempt.outcome.map(encode_attempt_outcome),
            attempt.error_reason,
        ],
    )?;
    Ok(())
}

/// 按主键读 Session。
///
/// **作用**：`get_session`，以及 `create_run` 在建 Run 前确认调用方给出的 Session 存在。
/// **边界**：没有行返回 `NotFound`。主键或时间戳无法解析返回 `Corrupt`，不造一个空 Session。
pub(crate) fn load_session(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Session, HostError> {
    let raw = connection
        .query_row(
            "SELECT session_id, created_at FROM sessions WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    session_from_raw(raw)
}

/// 按主键读 Run。
///
/// **作用**：状态查询、drive、取消、收尸和 `DrivePort` 都从这里取权威行。
/// **边界**：没有行返回 `NotFound`。状态、终态原因、预算或时间戳非法返回 `Corrupt`。不附带 Attempt 或 event。
pub(crate) fn load_run(connection: &Connection, run_id: &RunId) -> Result<Run, HostError> {
    let raw = connection
        .query_row(
            "SELECT run_id, session_id, status, terminal_reason, input_ref, workspace_ref,
                    max_model_calls, max_tool_calls, created_at, updated_at
             FROM runs WHERE run_id = ?1",
            params![run_id.as_str()],
            map_run_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    run_from_raw(raw)
}

/// 按主键读 Attempt。
///
/// **作用**：`RunHost::get_attempt`。驱动路径用 `latest_attempt`，不用这个函数选“当前”Attempt。
/// **边界**：没有行返回 `NotFound`。状态、结局或租约时间非法返回 `Corrupt`。
pub(crate) fn load_attempt(
    connection: &Connection,
    attempt_id: &AttemptId,
) -> Result<Attempt, HostError> {
    let raw = connection
        .query_row(
            "SELECT attempt_id, run_id, engine_id, engine_version, owner, lease_expires_at,
                    status, outcome, error_reason
             FROM attempts WHERE attempt_id = ?1",
            params![attempt_id.as_str()],
            map_attempt_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    attempt_from_raw(raw)
}

/// 按受理幂等键读已创建的 Run 及其最新 Attempt。
///
/// **作用**：`create_run` 在插入前、以及唯一约束冲突后，用来返回已有记录。
/// **参数**：`request_id` 允许任意非空字符串；是否合法由 `create_run` 事先检查。
/// **边界**：没有 Run 返回 `Ok(None)`。有 Run 但没有 Attempt 返回 `NotFound`。不比较这次请求的工作区或预算。
pub(crate) fn load_created_by_request_id(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<(Run, Attempt)>, HostError> {
    let Some(raw) = connection
        .query_row(
            "SELECT run_id, session_id, status, terminal_reason, input_ref, workspace_ref,
                    max_model_calls, max_tool_calls, created_at, updated_at
             FROM runs WHERE request_id = ?1",
            params![request_id],
            map_run_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    let run = run_from_raw(raw)?;
    let attempt = latest_attempt(connection, &run.run_id)?;
    Ok(Some((run, attempt)))
}

/// 取该 Run 下 `attempt_id` 最大的一行。
///
/// **作用**：快照、drive、取消、收尸和预算检查都把它当作当前 Attempt。A1.1 每个 Run 只有一行。
/// **边界**：没有行返回 `NotFound`。不按 `lease_expires_at` 或状态过滤；过期与否由调用方判断。
pub(crate) fn latest_attempt(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Attempt, HostError> {
    let raw = connection
        .query_row(
            "SELECT attempt_id, run_id, engine_id, engine_version, owner, lease_expires_at,
                    status, outcome, error_reason
             FROM attempts WHERE run_id = ?1
             ORDER BY attempt_id DESC LIMIT 1",
            params![run_id.as_str()],
            map_attempt_row,
        )
        .optional()?
        .ok_or(HostError::NotFound)?;
    attempt_from_raw(raw)
}

/// 在同一事务里更新 Run 与 Attempt 行。
///
/// **作用**：状态变更、续期、取消、收尸共用；Attempt 的 `lease_expires_at` 一并写入。
/// **参数**：`run` / `attempt` 是调用方已改好的内存快照，本函数不做转移校验。
/// **边界**：任一行影响行数不是 1 返回 `NotFound`。不写 durable event。
pub(crate) fn update_run_and_attempt(
    transaction: &Transaction<'_>,
    run: &Run,
    attempt: &Attempt,
) -> Result<(), HostError> {
    let changed = transaction.execute(
        "UPDATE runs
         SET status = ?1, terminal_reason = ?2, updated_at = ?3
         WHERE run_id = ?4",
        params![
            encode_run_status(run.status),
            run.terminal_reason.map(encode_terminal_reason),
            run.updated_at.as_str(),
            run.run_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(HostError::NotFound);
    }
    let changed = transaction.execute(
        "UPDATE attempts
         SET status = ?1, outcome = ?2, error_reason = ?3, lease_expires_at = ?4
         WHERE attempt_id = ?5",
        params![
            encode_attempt_status(attempt.status),
            attempt.outcome.map(encode_attempt_outcome),
            attempt.error_reason,
            attempt.lease_expires_at.as_str(),
            attempt.attempt_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(HostError::NotFound);
    }
    Ok(())
}

/// 列出可能需要收孤儿的 Attempt：所属 Run 为 `Running` / `Cancelling`，Attempt 仍为 `Running`。
///
/// **作用**：启动扫描的候选集；调用方再按 `lease_expires_at` 过滤。
/// **边界**：不在 SQL 里比时间，避免时区/格式分歧；调用方用 [`crate::time::lease_is_expired`]。
pub(crate) fn list_active_drive_attempts(
    connection: &Connection,
) -> Result<Vec<(Run, Attempt)>, HostError> {
    let mut statement = connection.prepare(
        "SELECT r.run_id, r.session_id, r.status, r.terminal_reason, r.input_ref, r.workspace_ref,
                r.max_model_calls, r.max_tool_calls, r.created_at, r.updated_at,
                a.attempt_id, a.run_id, a.engine_id, a.engine_version, a.owner, a.lease_expires_at,
                a.status, a.outcome, a.error_reason
         FROM runs r
         INNER JOIN attempts a ON a.run_id = r.run_id
         WHERE r.status IN ('Running', 'Cancelling')
           AND a.status = 'Running'
         ORDER BY r.run_id, a.attempt_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            RunRow {
                run_id: row.get(0)?,
                session_id: row.get(1)?,
                status: row.get(2)?,
                terminal_reason: row.get(3)?,
                input_ref: row.get(4)?,
                workspace_ref: row.get(5)?,
                max_model_calls: row.get(6)?,
                max_tool_calls: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            },
            AttemptRow {
                attempt_id: row.get(10)?,
                run_id: row.get(11)?,
                engine_id: row.get(12)?,
                engine_version: row.get(13)?,
                owner: row.get(14)?,
                lease_expires_at: row.get(15)?,
                status: row.get(16)?,
                outcome: row.get(17)?,
                error_reason: row.get(18)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (run_raw, attempt_raw) = row?;
        out.push((run_from_raw(run_raw)?, attempt_from_raw(attempt_raw)?));
    }
    Ok(out)
}

/// 读当前 `journal_mode`，供打开库后的测试断言。
///
/// **作用**：`RunHost::journal_mode` 转发到这里。
/// **边界**：只在测试构建存在。不修改 pragma。
#[cfg(test)]
pub(crate) fn journal_mode(connection: &Connection) -> Result<String, HostError> {
    Ok(connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
}

/// 读当前 `busy_timeout`（毫秒），供打开库后的测试断言。
///
/// **作用**：`RunHost::busy_timeout_ms` 转发到这里。
/// **边界**：只在测试构建存在。不修改 pragma。
#[cfg(test)]
pub(crate) fn busy_timeout_ms(connection: &Connection) -> Result<i64, HostError> {
    Ok(connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?)
}

/// `runs` 表的一行原文，尚未做主键和枚举校验。
struct RunRow {
    run_id: String,
    session_id: Option<String>,
    status: String,
    terminal_reason: Option<String>,
    input_ref: String,
    workspace_ref: String,
    max_model_calls: i64,
    max_tool_calls: i64,
    created_at: String,
    updated_at: String,
}

/// `attempts` 表的一行原文，尚未做主键和枚举校验。
struct AttemptRow {
    attempt_id: String,
    run_id: String,
    engine_id: String,
    engine_version: String,
    owner: String,
    lease_expires_at: String,
    status: String,
    outcome: Option<String>,
    error_reason: Option<String>,
}

/// 把 `runs` 的 SELECT 列收成 [`RunRow`]。
///
/// **作用**：`load_run`、`load_created_by_request_id` 共用列顺序。
/// **边界**：只传递 SQLite 读错误。非法状态留到 `run_from_raw`。
fn map_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: row.get(0)?,
        session_id: row.get(1)?,
        status: row.get(2)?,
        terminal_reason: row.get(3)?,
        input_ref: row.get(4)?,
        workspace_ref: row.get(5)?,
        max_model_calls: row.get(6)?,
        max_tool_calls: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// 把 `attempts` 的 SELECT 列收成 [`AttemptRow`]。
///
/// **作用**：`load_attempt` 与 `latest_attempt` 共用列顺序。
/// **边界**：只传递 SQLite 读错误。非法状态留到 `attempt_from_raw`。
fn map_attempt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRow> {
    Ok(AttemptRow {
        attempt_id: row.get(0)?,
        run_id: row.get(1)?,
        engine_id: row.get(2)?,
        engine_version: row.get(3)?,
        owner: row.get(4)?,
        lease_expires_at: row.get(5)?,
        status: row.get(6)?,
        outcome: row.get(7)?,
        error_reason: row.get(8)?,
    })
}

/// 把 Session 两列解析成契约类型。
///
/// **作用**：`load_session` 的唯一解码点。
/// **边界**：`session_id` 或 `created_at` 不合格返回 `Corrupt`，字段名写在错误里。
fn session_from_raw((session_id, created_at): (String, String)) -> Result<Session, HostError> {
    Ok(Session {
        session_id: SessionId::parse(session_id).map_err(|_| HostError::Corrupt("session_id"))?,
        created_at: Timestamp::parse(created_at).map_err(|_| HostError::Corrupt("created_at"))?,
    })
}

/// 把 [`RunRow`] 解析成契约 `Run`。
///
/// **作用**：所有 Run 读取的唯一解码点。
/// **边界**：未知状态、未知终态原因、预算超出 `u32`、时间戳不合格都返回 `Corrupt`。不检查状态与 `terminal_reason` 是否配套。
fn run_from_raw(row: RunRow) -> Result<Run, HostError> {
    Ok(Run {
        run_id: RunId::parse(row.run_id).map_err(|_| HostError::Corrupt("run_id"))?,
        session_id: match row.session_id {
            Some(value) => {
                Some(SessionId::parse(value).map_err(|_| HostError::Corrupt("session_id"))?)
            }
            None => None,
        },
        status: decode_run_status(&row.status)?,
        terminal_reason: match row.terminal_reason {
            Some(value) => Some(decode_terminal_reason(&value)?),
            None => None,
        },
        input_ref: row.input_ref,
        workspace_ref: row.workspace_ref,
        budget: Budget {
            max_model_calls: parse_u32(row.max_model_calls)?,
            max_tool_calls: parse_u32(row.max_tool_calls)?,
        },
        created_at: Timestamp::parse(row.created_at)
            .map_err(|_| HostError::Corrupt("created_at"))?,
        updated_at: Timestamp::parse(row.updated_at)
            .map_err(|_| HostError::Corrupt("updated_at"))?,
    })
}

/// 把 [`AttemptRow`] 解析成契约 `Attempt`。
///
/// **作用**：所有 Attempt 读取的唯一解码点。
/// **边界**：未知状态或结局、主键或租约时间不合格返回 `Corrupt`。不检查租约是否已过期。
fn attempt_from_raw(row: AttemptRow) -> Result<Attempt, HostError> {
    Ok(Attempt {
        attempt_id: AttemptId::parse(row.attempt_id)
            .map_err(|_| HostError::Corrupt("attempt_id"))?,
        run_id: RunId::parse(row.run_id).map_err(|_| HostError::Corrupt("run_id"))?,
        engine_id: row.engine_id,
        engine_version: row.engine_version,
        owner: row.owner,
        lease_expires_at: Timestamp::parse(row.lease_expires_at)
            .map_err(|_| HostError::Corrupt("lease_expires_at"))?,
        status: decode_attempt_status(&row.status)?,
        outcome: match row.outcome {
            Some(value) => Some(decode_attempt_outcome(&value)?),
            None => None,
        },
        error_reason: row.error_reason,
    })
}

/// 把预算列从 SQLite 整数收成 `u32`。
///
/// **作用**：`run_from_raw` 解码 `max_model_calls` 与 `max_tool_calls`。
/// **边界**：负数或超过 `u32::MAX` 返回 `Corrupt("budget")`，不用 0 代替。
fn parse_u32(value: i64) -> Result<u32, HostError> {
    u32::try_from(value).map_err(|_| HostError::Corrupt("budget"))
}

/// Run 状态的库存文本：与枚举名相同的 PascalCase。
///
/// **作用**：`insert_created` 与 `update_run_and_attempt` 写入 `runs.status`。
fn encode_run_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Accepted => "Accepted",
        RunStatus::Running => "Running",
        RunStatus::Cancelling => "Cancelling",
        RunStatus::Completed => "Completed",
        RunStatus::Failed => "Failed",
        RunStatus::Cancelled => "Cancelled",
    }
}

/// 把 `runs.status` 还原成枚举。
///
/// **边界**：不是六种已知文本时返回 `Corrupt("run status")`，不默认成 `Failed` 或 `Accepted`。
fn decode_run_status(value: &str) -> Result<RunStatus, HostError> {
    match value {
        "Accepted" => Ok(RunStatus::Accepted),
        "Running" => Ok(RunStatus::Running),
        "Cancelling" => Ok(RunStatus::Cancelling),
        "Completed" => Ok(RunStatus::Completed),
        "Failed" => Ok(RunStatus::Failed),
        "Cancelled" => Ok(RunStatus::Cancelled),
        _ => Err(HostError::Corrupt("run status")),
    }
}

/// 终态原因的库存文本：小写蛇形，与事件里的 `terminal_reason` 用词一致。
///
/// **作用**：写入 `runs.terminal_reason`。`None` 由调用方省略，不经过这里。
fn encode_terminal_reason(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::Completed => "completed",
        TerminalReason::Cancelled => "cancelled",
        TerminalReason::BudgetExhausted => "budget_exhausted",
        TerminalReason::HostRestart => "host_restart",
        TerminalReason::EngineError => "engine_error",
        TerminalReason::ProviderError => "provider_error",
    }
}

/// 把 `runs.terminal_reason` 还原成枚举。
///
/// **边界**：未知文本返回 `Corrupt("terminal reason")`。空列由调用方当成 `None`，不进这个函数。
fn decode_terminal_reason(value: &str) -> Result<TerminalReason, HostError> {
    match value {
        "completed" => Ok(TerminalReason::Completed),
        "cancelled" => Ok(TerminalReason::Cancelled),
        "budget_exhausted" => Ok(TerminalReason::BudgetExhausted),
        "host_restart" => Ok(TerminalReason::HostRestart),
        "engine_error" => Ok(TerminalReason::EngineError),
        "provider_error" => Ok(TerminalReason::ProviderError),
        _ => Err(HostError::Corrupt("terminal reason")),
    }
}

/// Attempt 状态的库存文本：与枚举名相同的 PascalCase。
///
/// **作用**：写入 `attempts.status`。
fn encode_attempt_status(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Starting => "Starting",
        AttemptStatus::Running => "Running",
        AttemptStatus::Ended => "Ended",
        AttemptStatus::Orphaned => "Orphaned",
    }
}

/// 把 `attempts.status` 还原成枚举。
///
/// **边界**：未知文本返回 `Corrupt("attempt status")`。
fn decode_attempt_status(value: &str) -> Result<AttemptStatus, HostError> {
    match value {
        "Starting" => Ok(AttemptStatus::Starting),
        "Running" => Ok(AttemptStatus::Running),
        "Ended" => Ok(AttemptStatus::Ended),
        "Orphaned" => Ok(AttemptStatus::Orphaned),
        _ => Err(HostError::Corrupt("attempt status")),
    }
}

/// Attempt 结局的库存文本：小写蛇形。
///
/// **作用**：写入 `attempts.outcome`。进行中的 Attempt 没有结局，调用方传 `None`。
fn encode_attempt_outcome(outcome: AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Completed => "completed",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Cancelled => "cancelled",
    }
}

/// 把 `attempts.outcome` 还原成枚举。
///
/// **边界**：未知文本返回 `Corrupt("attempt outcome")`。空列由调用方当成 `None`。
fn decode_attempt_outcome(value: &str) -> Result<AttemptOutcome, HostError> {
    match value {
        "completed" => Ok(AttemptOutcome::Completed),
        "failed" => Ok(AttemptOutcome::Failed),
        "cancelled" => Ok(AttemptOutcome::Cancelled),
        _ => Err(HostError::Corrupt("attempt outcome")),
    }
}

/// 追加一条人工反馈（A4b）。
///
/// **作用**：`RunHost::add_feedback` 在确认 Run 存在后调用。
/// **边界**：不写 event、不改 `last_sequence`。`note` 已由调用方截断校验。
pub(crate) fn insert_feedback(
    connection: &Connection,
    feedback: &fluxaxiom_contract::SampleFeedback,
    run_id: &RunId,
) -> Result<(), HostError> {
    connection.execute(
        "INSERT INTO sample_feedback (feedback_id, run_id, created_at, verdict, note)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            feedback.feedback_id.as_str(),
            run_id.as_str(),
            feedback.created_at.as_str(),
            encode_verdict(feedback.verdict),
            feedback.note.as_deref(),
        ],
    )?;
    Ok(())
}

/// 按 `created_at`、`feedback_id` 升序列出该 Run 的全部反馈。
///
/// **作用**：样本导出与反馈验收读回。
/// **边界**：没有行时返回空 Vec，不报 `NotFound`。
pub(crate) fn list_feedback(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Vec<fluxaxiom_contract::SampleFeedback>, HostError> {
    let mut statement = connection.prepare(
        "SELECT feedback_id, created_at, verdict, note FROM sample_feedback
         WHERE run_id = ?1
         ORDER BY created_at ASC, feedback_id ASC",
    )?;
    let rows = statement.query_map(params![run_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, created_at, verdict, note) = row?;
        out.push(fluxaxiom_contract::SampleFeedback {
            feedback_id: fluxaxiom_contract::FeedbackId::parse(id)
                .map_err(|_| HostError::Corrupt("feedback_id"))?,
            created_at: fluxaxiom_contract::Timestamp::parse(created_at)
                .map_err(|_| HostError::Corrupt("feedback created_at"))?,
            verdict: decode_verdict(&verdict)?,
            note,
        });
    }
    Ok(out)
}

/// 反馈总评的库存文本：`usable` / `needs_edit` / `redo`。
///
/// **作用**：`insert_feedback` 写入 `sample_feedback.verdict`。
fn encode_verdict(verdict: fluxaxiom_contract::FeedbackVerdict) -> &'static str {
    match verdict {
        fluxaxiom_contract::FeedbackVerdict::Usable => "usable",
        fluxaxiom_contract::FeedbackVerdict::NeedsEdit => "needs_edit",
        fluxaxiom_contract::FeedbackVerdict::Redo => "redo",
    }
}

/// 把 `sample_feedback.verdict` 还原成枚举。
///
/// **边界**：未知文本返回 `Corrupt("feedback verdict")`，不默认成 `redo`。
fn decode_verdict(value: &str) -> Result<fluxaxiom_contract::FeedbackVerdict, HostError> {
    match value {
        "usable" => Ok(fluxaxiom_contract::FeedbackVerdict::Usable),
        "needs_edit" => Ok(fluxaxiom_contract::FeedbackVerdict::NeedsEdit),
        "redo" => Ok(fluxaxiom_contract::FeedbackVerdict::Redo),
        _ => Err(HostError::Corrupt("feedback verdict")),
    }
}
