//! Deterministic remote Worker fixture for the Hand/Brain lazy Sandbox E2E.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult, ToolCall,
};
use awaken_server::InferenceExecutorMaterializer;

struct HandBrainModel;

fn text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolResult { content, .. } => text(content),
            _ => String::new(),
        })
        .collect()
}

#[async_trait]
impl LlmExecutor for HandBrainModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let output = if let Some(last) = request.messages.last()
            && last.role == Role::Tool
        {
            // Leave the claimed dispatch observable after the tool result. The
            // E2E captures the authoritative in-flight Sandbox binding before
            // settlement removes the completed queue row.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            AssistantOutput::text(format!("tool-result: {}", text(&last.content)))
        } else {
            let prompt = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::User)
                .map(|message| text(&message.content))
                .unwrap_or_default();
            match prompt.as_str() {
                "brain" => AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: format!("remote-brain-{}", request.messages.len()),
                    tool_id: "mcp__calc__add".into(),
                    arguments: serde_json::json!({"a": 20, "b": 22}),
                }]),
                "hand" => AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: format!("remote-hand-{}", request.messages.len()),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({"path": "missing-remote-hand.txt"}),
                }]),
                _ => AssistantOutput::text(format!("Echo: {prompt}")),
            }
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
        .then(|| Arc::new(HandBrainModel) as Arc<dyn LlmExecutor>)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init(&Default::default());
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let storage = std::env::var("AWAKEN_TEST_WORKER_STORAGE_DIR")?;
    let tier = std::env::var("SESSION_ENVIRONMENT_TIER").unwrap_or_else(|_| "local".into());
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.durable = true;
    deployment.storage_dir = Some(storage.clone().into());
    deployment.sandbox_dir = Some(format!("{storage}/sandboxes").into());
    deployment.sandbox_tier = match tier.as_str() {
        "local" => awaken_runtime_host::SandboxTier::Local,
        "namespace" => awaken_runtime_host::SandboxTier::Namespace,
        "docker" => awaken_runtime_host::SandboxTier::Docker,
        "podman" => awaken_runtime_host::SandboxTier::Podman,
        other => return Err(format!("unsupported E2E sandbox tier `{other}`").into()),
    };
    if matches!(tier.as_str(), "docker" | "podman") {
        deployment.container_image = Some(
            std::env::var("AWAKEN_TEST_SESSION_IMAGE")
                .unwrap_or_else(|_| "awaken-sandbox:session-e2e".into()),
        );
    }
    // Environment-managed Sessions always carry a (possibly empty) frozen
    // resource envelope. Install the real Resource plane and validator so
    // the standard manifest may honestly advertise session-resources/v1; the
    // coordinator will otherwise reject this Worker before it can claim.
    let resources = awaken_worker::WorkerSessionResourceAdapters::new(
        awaken_server::embedded_skill_store(&std::path::Path::new(&storage).join("resources")),
        Arc::new(awaken_admin_config_api::SqliteAdminStore::open_in_memory()?),
    );
    awaken_worker::WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new(upstream))
        .with_inference_materializer(Arc::new(HostMaterializer))
        .with_deployment_config(deployment)
        .with_session_resource_adapters(resources)
        .with_registered_memory_mounter_factory(awaken_cli::registered_memory_mounter_factory())
        .with_standard_manifest(Default::default())
        .without_admin_surface()
        .build()?
        .run_until_shutdown()
        .await
}
