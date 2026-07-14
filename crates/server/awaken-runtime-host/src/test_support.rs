//! Shared `#[cfg(test)]` fixtures for the host's cloud-managed-gateway tests, so the
//! `run_exec` (direct path) and `session` (durable path) test modules share one set
//! of stubs instead of each redeclaring them.
#![cfg(test)]

use std::sync::Arc;

use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::model_access::{ModelAccessGrant, ResolvedModelEndpoint};

use crate::gateway_executor::GatewayExecutorFactory;

/// A model that is never actually invoked in these decision tests.
pub(crate) struct StubModel;

#[async_trait::async_trait]
impl LlmExecutor for StubModel {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("stub"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A gateway factory that serves any endpoint (returns a stub executor).
pub(crate) struct ServingFactory;
impl GatewayExecutorFactory for ServingFactory {
    fn build(&self, _e: &ResolvedModelEndpoint) -> Option<Arc<dyn LlmExecutor>> {
        Some(Arc::new(StubModel))
    }
}

/// A gateway factory that cannot serve the dialect (returns `None`).
pub(crate) struct UnservingFactory;
impl GatewayExecutorFactory for UnservingFactory {
    fn build(&self, _e: &ResolvedModelEndpoint) -> Option<Arc<dyn LlmExecutor>> {
        None
    }
}

/// A canonical cloud-managed gateway grant for tests.
pub(crate) fn gateway_grant() -> ModelAccessGrant {
    ModelAccessGrant::CloudManagedGateway {
        gateway_base_url: "https://gw.internal".into(),
        dialect: "anthropic".into(),
        model_ref: "claude".into(),
        lease_token: "lease".into(), // awaken-allow: secret
    }
}
