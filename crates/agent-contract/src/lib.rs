//! P0a draft contract types. `schema_version` `0.3-draft` is not Contract v1.

mod artifact;
mod attempt;
mod error;
mod event;
mod id;
mod run;
mod session;
mod snapshot;
mod time_stamp;

pub use artifact::Artifact;
pub use attempt::{attempt_transition_allowed, Attempt, AttemptOutcome, AttemptStatus};
pub use error::Error;
pub use event::{
    ArtifactCreatedPayload, AttemptEndedPayload, AttemptOrphanedPayload, AttemptStartedPayload,
    EmptyPayload, Event, EventBody, ModelCallCompletedPayload, ModelUsage, RunTerminalPayload,
    TokenCount, ToolCallCompletedPayload,
};
pub use id::{ArtifactId, AttemptId, EventId, IdError, RunId, SessionId};
pub use run::{run_transition_allowed, Budget, Run, RunStatus, TerminalReason};
pub use session::Session;
pub use snapshot::RunSnapshot;
pub use time_stamp::{Timestamp, TimestampError};

pub const SCHEMA_VERSION: &str = "0.3-draft";
