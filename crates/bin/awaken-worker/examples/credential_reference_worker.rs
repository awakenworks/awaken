//! E2E fixture: a worker that materializes one opaque credential reference into
//! an executor. Endpoint selection is already fixed; only credential injection is
//! exercised, independent of deployment topology.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::InferenceAccess;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_server::InferenceExecutorMaterializer;

struct GrantExecutor {
    reference: String,
}

#[async_trait]
impl LlmExecutor for GrantExecutor {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("credential-reference:{}", self.reference)),
            usage: None,
            stop_reason: None,
        })
    }
}

struct ReferenceMaterializer;

impl InferenceExecutorMaterializer for ReferenceMaterializer {
    fn materialize_pinned(
        &self,
        _model_ref: &str,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        (access.scheme == "credential-reference/v1").then(|| {
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
    awaken_worker::run_with_inference_materializer(&upstream, Arc::new(ReferenceMaterializer)).await
}
