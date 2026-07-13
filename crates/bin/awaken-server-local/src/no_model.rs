//! The production "no model configured" fallback executor.
//!
//! The management assembly needs a host default model for the window before an
//! operator has authored + published one through `/v1/config/*` + `/v1/vaults/*`.
//! In production that default must NOT be a mock (an echo/tool scenario model
//! belongs in tests only) — it returns a clear, actionable message instead, so a
//! session that runs before a model is configured gets guidance rather than a
//! surprising echo. Once a model is published, `ConfigExecutorProvider` resolves
//! the real provider per session and this fallback is never reached for that agent.

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};

/// The model ref this fallback registers under (a stable, non-provider token).
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
