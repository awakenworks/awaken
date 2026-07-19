//! E2E fixture: a secretless worker whose injected provider resolves one opaque
//! model-access grant into an executor. The executor echoes only the non-secret
//! grant reference, making provider routing observable to an external TS test.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_host::ModelAccessRef;
use awaken_server::ExecutorProvider;

struct GrantExecutor {
    reference: String,
}

#[async_trait]
impl LlmExecutor for GrantExecutor {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("gateway-grant:{}", self.reference)),
            usage: None,
            stop_reason: None,
        })
    }
}

struct GatewayProvider;

impl ExecutorProvider for GatewayProvider {
    fn executor_for(&self, _model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        None
    }

    fn executor_for_run(
        &self,
        _model_ref: &str,
        model_access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let access = model_access?;
        (access.scheme == "cloud-gateway").then(|| {
            Arc::new(GrantExecutor {
                reference: access.reference.clone(),
            }) as Arc<dyn LlmExecutor>
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    awaken_observability::init();
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    awaken_worker::run_with_executor_provider(&upstream, Arc::new(GatewayProvider)).await
}
