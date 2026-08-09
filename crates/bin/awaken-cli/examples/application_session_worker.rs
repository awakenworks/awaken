//! E2E composition fixture for the public application-factory seam.
//!
//! The executable contains no alternate Session protocol: it installs one
//! application provisioner into the production `WorkerNodeBuilder`, then lets
//! the ordinary registered Worker transport contribute and realize the plan.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::{content::ContentBlock, message::Role};
use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult, ToolCall,
};
use awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier;

const APPLICATION_PROMPT: &str = "application-session-e2e-prompt";

struct ProjectionExecutor;

fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolResult { content, .. } => content_text(content),
            _ => String::new(),
        })
        .collect()
}

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
        let last_is_tool_result = request
            .messages
            .last()
            .is_some_and(|message| message.role == Role::Tool);
        let renewed_mcp_requested = request.messages.iter().any(|message| {
            message.role == Role::User
                && content_text(&message.content).contains("exercise renewed application MCP")
        });
        let output = if last_is_tool_result {
            AssistantOutput::text(format!(
                "application-session-renewed-mcp:{}",
                content_text(&request.messages.last().expect("checked above").content)
            ))
        } else if renewed_mcp_requested {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("application-renewed-mcp-{}", request.messages.len()),
                tool_id: "mcp__application_only__add".into(),
                arguments: serde_json::json!({"a": 20, "b": 22}),
            }])
        } else {
            AssistantOutput::text(format!(
                "application-session-projection:{}",
                if prompt_is_visible {
                    "visible"
                } else {
                    "missing"
                }
            ))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct HostMaterializer;

impl InferenceExecutorMaterializer for HostMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[awaken_run_ingress::HOST_EXECUTOR_CAPABILITY]
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
impl awaken_session_contract::ApplicationSessionProvisioner for ApplicationProvisioner {
    async fn prepare(
        &self,
        _activation: &awaken_runtime_contract::activation::RunActivation,
        session_id: &str,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<
        awaken_session_contract::ApplicationSessionContribution,
        awaken_session_contract::ApplicationSessionProvisionError,
    > {
        // The provisioner participates in the same claim boundary as the Host;
        // checking here proves an embedding application can fence its own I/O.
        ownership.verify_current().await.map_err(|error| {
            awaken_session_contract::ApplicationSessionProvisionError::new(error.to_string())
        })?;
        let mcp_url = std::env::var("AWAKEN_TEST_MCP_URL").map_err(|error| {
            awaken_session_contract::ApplicationSessionProvisionError::new(format!(
                "AWAKEN_TEST_MCP_URL is required: {error}"
            ))
        })?;
        let env = awaken_provisioning_contract::EnvVar {
            name: "APPLICATION_CONTRIBUTION_VISIBLE".into(),
            value: awaken_provisioning_contract::EnvValue::Inline {
                value: "yes".into(),
            },
            visibility: awaken_provisioning_contract::EnvVisibility::Process,
        };
        Ok(awaken_session_contract::ApplicationSessionContribution {
            session_id: session_id.to_owned(),
            application_fingerprint: format!("application-session-e2e-v2:{mcp_url}"),
            input: awaken_session_contract::ApplicationSessionInput {
                env: vec![serde_json::to_value(env).map_err(|error| {
                    awaken_session_contract::ApplicationSessionProvisionError::new(
                        error.to_string(),
                    )
                })?],
                prompts: vec![APPLICATION_PROMPT.to_string()],
                mcp_inputs: vec![
                    serde_json::json!({
                        "name": "application-calc",
                        "type": "url",
                        "url": mcp_url.clone(),
                    }),
                    serde_json::json!({
                        "name": "application-only",
                        "type": "url",
                        "url": mcp_url,
                    }),
                ],
                network_restriction: Some(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                ),
                ..Default::default()
            },
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init(&Default::default());
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let decorator: awaken_runtime_host::AttemptExecutorDecorator = Arc::new(|inner| inner);
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    if let Ok(tier) = std::env::var("AWAKEN_TEST_SANDBOX_TIER") {
        deployment.sandbox_tier = match tier.as_str() {
            "local" => awaken_runtime_host::SandboxTier::Local,
            _ => return Err(format!("unsupported E2E sandbox tier `{tier}`").into()),
        };
    }
    awaken_worker::WorkerNodeBuilder::new(awaken_worker_transport_security::WorkerUpstream::new(
        upstream,
    ))
    .with_deployment_config(deployment)
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
