//! Attempt：一次 Run 上由 Host 创建并驱动的 Engine 执行尝试。
//!
//! Host 独占写入；Engine 不直接改 Attempt 行。状态与 `attempt.*` durable event 在 Host 事务里对齐（ADR-014）。
//! 租约字段自 A1.1 起存在；续期、过期扫描与 Ctrl-C 接线见 ADR-021（A3b）。

use serde::{Deserialize, Serialize};

use crate::id::{AttemptId, RunId};
use crate::time_stamp::Timestamp;

/// Attempt 生命周期状态（JSON 与 schema 使用 PascalCase 枚举名）。
///
/// - `Starting`：已创建，尚未进入 `Running`（含 `prepare_drive` 失败前）。
/// - `Running`：Host 已提交运行态，Engine 可能正在 `drive_run`。
/// - `Ended`：正常收尾；必须带 [`AttemptOutcome`]。
/// - `Orphaned`：租约过期且进程未正常结束（如强杀后扫描）；无 outcome，见 ADR-021 §4。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptStatus {
    Starting,
    Running,
    Ended,
    Orphaned,
}

/// Attempt 进入 `Ended` 时的结果类别（JSON `snake_case`）。
///
/// 与 Run 的 [`crate::TerminalReason`] 相关但不一一同名：Attempt 只描述这一次 try 如何结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// 持久化的 Attempt 记录。形状与 `contracts/schemas/attempt.schema.json` 一致。
///
/// - `engine_id` / `engine_version`：Host 在创建 Attempt 时写入的 Engine 实现身份，不是权限。
/// - `owner`：持有租约的 Host 进程标识（如 `pid-*`），供过期扫描判断孤儿。
/// - `lease_expires_at`：创建时写入 TTL；A3b 前 Host 不续期。
/// - `outcome` / `error_reason`：仅 `status == Ended` 时 outcome 必填；`error_reason` 为可选简短类别，非堆栈。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub attempt_id: AttemptId,
    pub run_id: RunId,
    pub engine_id: String,
    pub engine_version: String,
    pub owner: String,
    pub lease_expires_at: Timestamp,
    pub status: AttemptStatus,
    pub outcome: Option<AttemptOutcome>,
    pub error_reason: Option<String>,
}

impl Attempt {
    /// 与 JSON Schema `allOf` 规则一致的内存校验（测试与样例用，不替代 Host 写入前检查）。
    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        let ended = self.status == AttemptStatus::Ended;
        ended == self.outcome.is_some()
            && !self.engine_id.is_empty()
            && !self.engine_version.is_empty()
            && !self.owner.is_empty()
            && self
                .error_reason
                .as_ref()
                .is_none_or(|reason| !reason.is_empty())
    }
}

/// P0a 允许的 Attempt 状态迁移。Host 在改 status 前应调用；非法迁移不得落库。
///
/// 不含 `Ended` / `Orphaned` → 任意：终态不可重启。`Starting → Ended` 用于从未进入 `Running` 的失败路径（ADR-021 §1）。
#[must_use]
pub fn attempt_transition_allowed(from: AttemptStatus, to: AttemptStatus) -> bool {
    matches!(
        (from, to),
        (AttemptStatus::Starting, AttemptStatus::Running)
            // 从未进入 Running 的 Attempt 可直接 Ended（prepare_drive 失败，见 ADR-021）。
            | (AttemptStatus::Starting, AttemptStatus::Ended)
            | (AttemptStatus::Starting, AttemptStatus::Orphaned)
            | (AttemptStatus::Running, AttemptStatus::Ended)
            | (AttemptStatus::Running, AttemptStatus::Orphaned)
    )
}

#[cfg(test)]
mod tests {
    use super::{attempt_transition_allowed, AttemptStatus};

    #[test]
    fn allows_p0a_attempt_transitions() {
        assert!(attempt_transition_allowed(
            AttemptStatus::Starting,
            AttemptStatus::Running
        ));
        assert!(attempt_transition_allowed(
            AttemptStatus::Starting,
            AttemptStatus::Ended
        ));
        assert!(attempt_transition_allowed(
            AttemptStatus::Running,
            AttemptStatus::Orphaned
        ));
    }

    #[test]
    fn rejects_ended_restart() {
        assert!(!attempt_transition_allowed(
            AttemptStatus::Ended,
            AttemptStatus::Running
        ));
        assert!(!attempt_transition_allowed(
            AttemptStatus::Orphaned,
            AttemptStatus::Ended
        ));
    }
}
