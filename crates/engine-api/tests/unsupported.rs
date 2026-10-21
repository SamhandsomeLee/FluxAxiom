use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use fluxaxiom_contract::RunId;
use fluxaxiom_engine_api::{
    ArtifactDraft, BlobRef, BudgetExhausted, BudgetLease, BudgetRequest, EffectOutcome,
    EngineAdapter, EngineDescriptor, EngineObservation, EngineOutcome, EngineRunRequest, HostPort,
    HostPortError, InputOutcome, InputRequest, ProposedEffect,
};
use tokio_util::sync::CancellationToken;

struct StubHost {
    cancel: CancellationToken,
}

#[async_trait]
impl HostPort for StubHost {
    fn observe(&self, _observation: EngineObservation) -> Result<(), HostPortError> {
        Err(HostPortError::Unsupported)
    }

    fn put_blob(&self, _bytes: Bytes, _media_type: &str) -> Result<BlobRef, HostPortError> {
        Err(HostPortError::Unsupported)
    }

    fn reserve(&self, _request: BudgetRequest) -> Result<BudgetLease, BudgetExhausted> {
        Err(BudgetExhausted)
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    async fn propose_effect(&self, _effect: ProposedEffect) -> EffectOutcome {
        EffectOutcome::Unsupported
    }

    async fn request_input(&self, _request: InputRequest) -> InputOutcome {
        InputOutcome::Unsupported
    }
}

struct ImmediateEngine;

#[async_trait]
impl EngineAdapter for ImmediateEngine {
    fn descriptor(&self) -> EngineDescriptor {
        EngineDescriptor {
            engine_id: "engine-a".to_owned(),
            version: "0.1.0".to_owned(),
        }
    }

    async fn run(&self, _request: EngineRunRequest, _host: Arc<dyn HostPort>) -> EngineOutcome {
        EngineOutcome::Completed {
            report: ArtifactDraft {
                artifact_type: "report".to_owned(),
                summary: "stub".to_owned(),
            },
        }
    }
}

#[tokio::test]
async fn p0a_effect_and_input_are_unsupported() {
    let host: Arc<dyn HostPort> = Arc::new(StubHost {
        cancel: CancellationToken::new(),
    });
    assert_eq!(
        host.propose_effect(ProposedEffect {
            tool_id: "workspace.write_file".to_owned(),
        })
        .await,
        EffectOutcome::Unsupported
    );
    assert_eq!(
        host.request_input(InputRequest {
            prompt: "continue?".to_owned(),
        })
        .await,
        InputOutcome::Unsupported
    );
}

#[tokio::test]
async fn engine_adapter_accepts_host_port_object() {
    let host: Arc<dyn HostPort> = Arc::new(StubHost {
        cancel: CancellationToken::new(),
    });
    let outcome = ImmediateEngine
        .run(
            EngineRunRequest {
                run_id: RunId::generate(),
                input: "analyze runtime".to_owned(),
            },
            host,
        )
        .await;
    assert!(matches!(outcome, EngineOutcome::Completed { .. }));
}
