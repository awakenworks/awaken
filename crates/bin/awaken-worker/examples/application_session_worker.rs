//! E2E composition fixture for the public application-factory seam.
//!
//! The executable contains no alternate Session protocol: it installs one
//! application provisioner into the production `WorkerNodeBuilder`, then lets
//! the ordinary registered Worker transport contribute and realize the plan.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier;
use awaken_server::InferenceExecutorMaterializer;

const APPLICATION_PROMPT: &str = "application-session-e2e-prompt";

struct ProjectionExecutor;

#[async_trait]
impl LlmExecutor for ProjectionExecutor {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let prompt_is_visible = request.messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    awaken_agent_contract::agent::content::ContentBlock::Text { text }
                        if text.contains(APPLICATION_PROMPT)
                )
            })
        });
        Ok(ChatResponse {
            output: AssistantOutput::text(format!(
                "application-session-projection:{}",
                if prompt_is_visible {
                    "visible"
                } else {
                    "missing"
                }
            )),
            usage: None,
            stop_reason: None,
        })
    }
}

struct HostMaterializer;

impl InferenceExecutorMaterializer for HostMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[awaken_runtime_host::HOST_EXECUTOR_CAPABILITY]
    }

    fn materialize_pinned(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        matches!(
            candidate.provisioning,
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        )
        .then(|| Arc::new(ProjectionExecutor) as Arc<dyn LlmExecutor>)
    }
}

struct ApplicationProvisioner;

#[async_trait]
impl awaken_runtime_host::ApplicationSessionProvisioner for ApplicationProvisioner {
    async fn prepare(
        &self,
        _activation: &awaken_runtime_contract::activation::RunActivation,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<
        awaken_runtime_host::ApplicationSessionPlan,
        awaken_runtime_host::ApplicationSessionError,
    > {
        // The provisioner participates in the same claim boundary as the Host;
        // checking here proves an embedding application can fence its own I/O.
        ownership.verify_current().await.map_err(|error| {
            awaken_runtime_host::ApplicationSessionError::new(error.to_string())
        })?;
        let mut plan =
            awaken_runtime_host::ApplicationSessionPlan::empty("application-session-e2e-v1");
        plan.prompts.push(APPLICATION_PROMPT.to_string());
        Ok(plan)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init();
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let decorator: awaken_runtime_host::AttemptExecutorDecorator = Arc::new(|inner| inner);
    awaken_worker::WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new(upstream))
        .with_inference_materializer(Arc::new(HostMaterializer))
        .with_application_factory(Arc::new(move |_| {
            Ok(
                awaken_worker::RegisteredWorkerApplication::new(decorator.clone())
                    .with_session_provisioner(Arc::new(ApplicationProvisioner)),
            )
        }))
        .with_standard_manifest(Default::default())
        .without_admin_surface()
        .build()?
        .run_until_shutdown()
        .await
}
