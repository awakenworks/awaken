//! Provider-free placeholder for a host with no executable model.
//!
//! `SharedHost` needs an executor for bare/local composition and auxiliary setup.
//! A production management host also installs `CredentialInferenceMaterializer`;
//! every Session candidate must then materialize from its publication pin and a
//! failure is rejected before this placeholder can run. It is not a publishable
//! default, provider selection, or credential fallback.

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};

/// Stable, non-provider identity of the inert placeholder.
pub const UNCONFIGURED_MODEL_REF: &str = "unconfigured";

/// A deterministic, provider-free executor that ends the turn with one line of
/// guidance. It performs no inference and calls no tools.
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

    #[tokio::test]
    async fn infer_returns_actionable_guidance_and_never_a_mock_echo() {
        // The placeholder performs no inference: whatever the request, it ends
        // the turn with one line of guidance pointing at the config/vault surfaces —
        // never an echo of the input (which would be a mock leaking into production).
        let request = ChatRequest {
            model_binding: ModelBinding::new("id", UNCONFIGURED_MODEL_REF, "default"),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("echo me")],
            }],
            tools: vec![],
        };
        let resp = NoModelConfiguredExecutor
            .infer(request)
            .await
            .expect("the placeholder never errors");
        let text = resp.output.text_content();
        assert!(
            text.contains("/v1/config"),
            "points at config surface: {text}"
        );
        assert!(
            text.contains("/v1/vaults"),
            "points at the vault surface: {text}"
        );
        assert!(
            text.contains("No model is configured"),
            "leads with the reason: {text}"
        );
        assert!(
            !text.contains("echo me"),
            "never echoes the input (that would be a mock in production): {text}"
        );
        // A pure guidance turn: no usage accounting, no explicit stop reason, and it
        // requests no tool calls.
        assert!(resp.usage.is_none());
        assert!(resp.stop_reason.is_none());
        assert!(
            resp.output.tool_calls().is_empty(),
            "the placeholder calls no tools"
        );
    }

    #[test]
    fn the_unconfigured_model_ref_is_a_stable_non_provider_token() {
        // The placeholder ref is a fixed, provider-free token.
        assert_eq!(UNCONFIGURED_MODEL_REF, "unconfigured");
    }
}
