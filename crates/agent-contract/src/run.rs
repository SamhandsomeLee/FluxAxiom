use serde::{Deserialize, Serialize};

use crate::id::{RunId, SessionId};
use crate::time_stamp::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Accepted,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    Completed,
    Cancelled,
    BudgetExhausted,
    HostRestart,
    EngineError,
    ProviderError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub status: RunStatus,
    pub terminal_reason: Option<TerminalReason>,
    pub input_ref: String,
    pub workspace_ref: String,
    pub budget: Budget,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Run {
    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        let terminal = matches!(
            self.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        );
        terminal == self.terminal_reason.is_some()
            && !self.input_ref.is_empty()
            && !self.workspace_ref.is_empty()
    }
}

#[must_use]
pub fn run_transition_allowed(from: RunStatus, to: RunStatus) -> bool {
    matches!(
        (from, to),
        (RunStatus::Accepted, RunStatus::Running)
            | (RunStatus::Accepted, RunStatus::Failed)
            | (RunStatus::Accepted, RunStatus::Cancelling)
            | (RunStatus::Running, RunStatus::Completed)
            | (RunStatus::Running, RunStatus::Failed)
            | (RunStatus::Running, RunStatus::Cancelling)
            | (RunStatus::Cancelling, RunStatus::Cancelled)
            | (RunStatus::Cancelling, RunStatus::Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::{run_transition_allowed, RunStatus};

    #[test]
    fn allows_p0a_run_transitions() {
        assert!(run_transition_allowed(
            RunStatus::Accepted,
            RunStatus::Running
        ));
        assert!(run_transition_allowed(
            RunStatus::Running,
            RunStatus::Cancelling
        ));
        assert!(run_transition_allowed(
            RunStatus::Cancelling,
            RunStatus::Failed
        ));
        assert!(run_transition_allowed(
            RunStatus::Cancelling,
            RunStatus::Cancelled
        ));
    }

    #[test]
    fn rejects_terminal_restart() {
        assert!(!run_transition_allowed(
            RunStatus::Completed,
            RunStatus::Running
        ));
        assert!(!run_transition_allowed(
            RunStatus::Failed,
            RunStatus::Accepted
        ));
    }
}
