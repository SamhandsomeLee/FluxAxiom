//! Internal Engine–Host port. These types are not the public JSON contract.
//!
//! **作用**：Engine 与 Host 共用的 trait 和调用类型。两边 crate 只依赖这里，互不依赖。
//!
//! **边界**：不写 SQLite，不保存 blob。`observe` 的校验和落库在 `run-host`。

mod adapter;
mod host;
mod types;

pub use adapter::EngineAdapter;
pub use fluxaxiom_contract::{ModelUsage, TokenCount};
pub use host::{HostPort, HostPortError};
pub use types::{
    ArtifactDraft, BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome,
    EngineDescriptor, EngineObservation, EngineOutcome, EngineRunRequest, FailureReason,
    InputOutcome, InputRequest, ProposedEffect,
};
