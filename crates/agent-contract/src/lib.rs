//! P0a draft contract types. `schema_version` `0.3-draft` is not Contract v1.

mod artifact;
mod attempt;
mod error;
mod event;
mod id;
mod run;
mod sample;
mod session;
mod snapshot;
mod time_stamp;

pub use artifact::Artifact;
pub use attempt::{attempt_transition_allowed, Attempt, AttemptOutcome, AttemptStatus};
pub use error::Error;
pub use event::{
    ArtifactCreatedPayload, AttemptEndedPayload, AttemptOrphanedPayload, AttemptStartedPayload,
    EmptyPayload, Event, EventBody, ModelCallCompletedPayload, ModelUsage, RunTerminalPayload,
    TokenCount, ToolCallCompletedPayload, WorkspaceRevision,
};
pub use id::{ArtifactId, AttemptId, EventId, FeedbackId, IdError, RunId, SessionId};
pub use run::{run_transition_allowed, Budget, Run, RunStatus, TerminalReason};
pub use sample::{
    FeedbackVerdict, RebuildResult, Sample, SampleArtifact, SampleCall, SampleFeedback,
    SampleModel, SampleRun, SampleToolCall, SampleTotals, SampleWorkspace,
};
pub use session::Session;
pub use snapshot::RunSnapshot;
pub use time_stamp::{Timestamp, TimestampError};

pub const SCHEMA_VERSION: &str = "0.3-draft";
