//! Internal Engine–Host port. These types are not the public JSON contract.

mod adapter;
mod host;
mod types;

pub use adapter::EngineAdapter;
pub use host::{HostPort, HostPortError};
pub use types::{
    ArtifactDraft, BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome,
    EngineDescriptor, EngineObservation, EngineOutcome, EngineRunRequest, FailureReason,
    InputOutcome, InputRequest, ProposedEffect,
};
