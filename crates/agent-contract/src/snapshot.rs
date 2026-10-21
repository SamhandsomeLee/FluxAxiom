use serde::{Deserialize, Serialize};

use crate::id::{ArtifactId, RunId, SessionId};
use crate::run::{Budget, RunStatus, TerminalReason};
use crate::time_stamp::Timestamp;
use crate::SCHEMA_VERSION;

/// Assembled from authoritative records. This struct is not a second source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub schema_version: String,
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub status: RunStatus,
    pub terminal_reason: Option<TerminalReason>,
    pub last_sequence: u64,
    pub input_ref: String,
    pub workspace_ref: String,
    pub budget: Budget,
    pub artifact_ids: Vec<ArtifactId>,
    pub updated_at: Timestamp,
}

impl RunSnapshot {
    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        let terminal = matches!(
            self.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        );
        self.schema_version == SCHEMA_VERSION
            && terminal == self.terminal_reason.is_some()
            && !self.input_ref.is_empty()
            && !self.workspace_ref.is_empty()
    }
}
