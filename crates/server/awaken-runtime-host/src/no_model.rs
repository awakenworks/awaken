//! Provider-free placeholder for a host with no executable model.
//!
//! A production host installs an exact inference materializer; every published
//! candidate must then materialize before this placeholder can be reached.

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};

/// Stable, non-provider identity of the inert placeholder.
pub const UNCONFIGURED_MODEL_REF: &str = "unconfigured";

/// A deterministic, provider-free executor that ends the turn with guidance.
pub struct NoModelConfiguredExecutor;

#[async_trait::async_trait]
impl LlmExecutor for NoModelConfiguredExecutor {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(
                "No model is configured. Author and publish a model (a provider, endpoint, \
                 offering, and credential) via /v1/config/* and /v1/vaults/*, then this agent \
                 will run on it."
                    .to_string(),
            ),
            usage: None,
            stop_reason: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::llm::{ChatMessage, ChatRequest};
    use awaken_runtime_contract::resolved::ModelBinding;

    /// Cause/effect rule: any request without an installed model produces one
    /// actionable, provider-free terminal message and never echoes user content.
    #[tokio::test]
    async fn infer_returns_actionable_guidance_and_never_a_mock_echo() {
        let request = ChatRequest {
            model_binding: ModelBinding::new("id", UNCONFIGURED_MODEL_REF, "default"),
            inference: Default::default(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("echo me")],
            }],
            tools: vec![],
        };
        let response = NoModelConfiguredExecutor.infer(request).await.unwrap();
        let text = response.output.text_content();
        assert!(text.contains("/v1/config") && text.contains("/v1/vaults"));
        assert!(text.contains("No model is configured"));
        assert!(!text.contains("echo me"));
        assert!(response.usage.is_none());
        assert!(response.stop_reason.is_none());
        assert!(response.output.tool_calls().is_empty());
    }
}
