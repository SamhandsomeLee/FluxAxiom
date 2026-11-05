//! 上下文管理。这一版提供 L0 前向去重、档案、估算和硬上限。
//!
//! **作用**：Engine 在每次模型调用前调用 [`ContextManager::prepare`]。校验器调用 [`ContextManager::replay`]。
//! **边界**：不依赖 `engine-api`、`engine-custom`、`run-host`。不读 blob，不改调用预算。
//! 不发网络请求。`mask@1` 的 L1、L2 不在这一版。
mod forward_dedup;
mod manager;
mod profile;
mod record;
mod strategy;
mod text;
mod transcript;

pub use forward_dedup::{ForwardDedup, L0_STRATEGY};
pub use manager::{ContextExhausted, ContextManager, Prepared};
pub use profile::{deepseek_flash, ModelProfile, ProfileError, ProfileField};
pub use record::{ContextRecord, ContextSpec, Estimate, EstimateSource, ReplayError};
pub use strategy::{ContextStrategy, Identity, IDENTITY_STRATEGY};
pub use transcript::{AssistantTurn, Round, ToolCall, ToolResult, Transcript, View};

#[cfg(test)]
mod tests {
    /// 本 crate 的依赖停在契约，不引用 Engine 或 Host。
    #[test]
    fn manifest_does_not_depend_on_engine_or_host() {
        let manifest = include_str!("../Cargo.toml");
        let start = manifest
            .find("[dependencies]")
            .expect("dependencies section");
        let rest = &manifest[start + "[dependencies]".len()..];
        let section = match rest.find("\n[") {
            Some(end) => &rest[..end],
            None => rest,
        };
        for name in [
            "fluxaxiom-engine-api",
            "fluxaxiom-engine-custom",
            "fluxaxiom-run-host",
        ] {
            assert!(!section.contains(name), "{name}");
        }
    }
}
