use serde::{Deserialize, Serialize};

use crate::id::{AttemptId, RunId};
use crate::time_stamp::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptStatus {
    Starting,
    Running,
    Ended,
    Orphaned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    Cancelled,
}

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

#[must_use]
pub fn attempt_transition_allowed(from: AttemptStatus, to: AttemptStatus) -> bool {
    matches!(
        (from, to),
        (AttemptStatus::Starting, AttemptStatus::Running)
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
