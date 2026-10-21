use std::sync::Arc;

use async_trait::async_trait;

use crate::host::HostPort;
use crate::types::{EngineDescriptor, EngineOutcome, EngineRunRequest};

#[async_trait]
pub trait EngineAdapter: Send + Sync {
    fn descriptor(&self) -> EngineDescriptor;
    async fn run(&self, request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome;
}
