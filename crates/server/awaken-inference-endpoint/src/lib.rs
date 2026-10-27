// @formal surface requirement=model.routing.exact_binding
// @formal reason: Provider executor construction for one materialized endpoint and the Awaken Cloud
// @formal reason: brokered grant transport; classified against the routing product requirement, not a
// @formal reason: formal-proof claim.
//! Inference executors for materialized model endpoints.
//!
//! A materialized endpoint is either a direct Provider endpoint whose
//! credential the caller already realized, or an exact short-lived grant the
//! Awaken Cloud broker issues ([`brokered_inference`], feature `brokered`).
//! This crate turns either into a live [`LlmExecutor`]; it selects no
//! credential and opens no Vault. Credential realization
//! (`awaken-credential-materializer`) and Control's Cloud catalog both build
//! on it, so the Control and Coordinator processes reach the broker without
//! linking the Worker's credential realization.
#![warn(unreachable_pub)]

mod inference_executor;
pub use inference_executor::InferenceExecutorMaterializer;

use std::sync::Arc;

use awaken_runtime_contract::UnspecifiedReasoning;
use awaken_runtime_contract::llm::LlmExecutor;

#[cfg(feature = "brokered")]
pub mod brokered_inference;

#[cfg(all(test, feature = "brokered"))]
pub(crate) struct NoopDeltaSink;

#[cfg(all(test, feature = "brokered"))]
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::DeltaSink for NoopDeltaSink {
    async fn on_text(&self, _: &str) {}
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolvedExecutorError {
    #[error("resolved inference for adapter `{0}` carries no base URL")]
    MissingBaseUrl(String),
    #[error("resolved inference carries no credential")]
    MissingCredential,
    #[error("no provider executor in this build serves adapter `{0}`")]
    UnsupportedAdapter(String),
    #[error("no provider executor in this build serves API dialect `{0}`")]
    UnsupportedDialect(String),
    #[error("API dialect `{dialect}` is incompatible with adapter `{adapter}`")]
    DialectAdapterMismatch { dialect: String, adapter: String },
    #[error("provider executor could not be constructed: {0}")]
    ExecutorBuild(String),
    #[error("API dialect `{0}` cannot enforce the frozen reasoning policy")]
    UnsupportedReasoningPolicy(String),
}

pub fn executor_from_materialized_endpoint(
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    executor_from_materialized_endpoint_for_provider(
        adapter_kind,
        api_dialect,
        adapter_kind,
        base_url,
        credential,
        UnspecifiedReasoning::ProviderDefault,
    )
}

/// Provider-aware endpoint realization. The provider kind is independent of
/// the wire adapter so compatible gateways cannot accidentally inherit native
/// capabilities from the protocol they emulate.
pub fn executor_from_materialized_endpoint_for_provider(
    provider_kind: &str,
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
    unspecified_reasoning: UnspecifiedReasoning,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    let expected_adapter = match api_dialect {
        "anthropic_messages" => "anthropic",
        "open_ai_chat" => "openai",
        "gemini" => "gemini",
        "vertex_gemini" => "vertex",
        "open_ai_responses" => "openai",
        other => return Err(ResolvedExecutorError::UnsupportedDialect(other.to_string())),
    };
    if expected_adapter != adapter_kind {
        return Err(ResolvedExecutorError::DialectAdapterMismatch {
            dialect: api_dialect.to_string(),
            adapter: adapter_kind.to_string(),
        });
    }
    let base_url =
        base_url.ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
    let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
    if api_dialect == "open_ai_responses" {
        if unspecified_reasoning != UnspecifiedReasoning::ProviderDefault {
            return Err(ResolvedExecutorError::UnsupportedReasoningPolicy(
                api_dialect.to_string(),
            ));
        }
        return awaken_provider_genai::OpenAiResponsesExecutor::from_materialized_endpoint(
            provider_kind,
            base_url,
            credential.expose_secret(),
        )
        .map(|executor| Arc::new(executor) as Arc<dyn LlmExecutor>)
        .map_err(|error| ResolvedExecutorError::ExecutorBuild(error.to_string()));
    }
    let adapter = match adapter_kind {
        "anthropic" => awaken_provider_genai::AdapterKind::Anthropic,
        "gemini" => awaken_provider_genai::AdapterKind::Gemini,
        "vertex" => awaken_provider_genai::AdapterKind::Vertex,
        "openai" => awaken_provider_genai::AdapterKind::OpenAI,
        other => return Err(ResolvedExecutorError::UnsupportedAdapter(other.to_string())),
    };
    awaken_provider_genai::GenaiExecutor::from_materialized_provider_endpoint(
        provider_kind,
        adapter,
        base_url,
        credential.expose_secret(),
        unspecified_reasoning,
    )
    .map(|executor| Arc::new(executor) as Arc<dyn LlmExecutor>)
    .map_err(|error| ResolvedExecutorError::ExecutorBuild(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;

    #[test]
    fn provider_executor_factory_decision_table() {
        // Causes: C1 dialect is present and supported, C2 dialect matches the
        // pinned adapter, C3 adapter is supported, C4 base URL exists, and C5
        // credential exists. Effects: E1 exact construction; E2 every false
        // cause fails closed before provider I/O. Decision rules:
        // R1=C1+C2+C3+C4+C5=>E1; R2=!C1=>UnsupportedDialect; each other single
        // false cause yields its typed error. Empty historical dialect is not an
        // executable compatibility path; it must be migrated at publication.
        let credential = RedactedString::new("sk-test");
        for (dialect, adapter) in [
            ("anthropic_messages", "anthropic"),
            ("open_ai_chat", "openai"),
            ("open_ai_responses", "openai"),
            ("gemini", "gemini"),
            ("vertex_gemini", "vertex"),
        ] {
            assert!(
                executor_from_materialized_endpoint(
                    dialect,
                    adapter,
                    Some("https://provider.invalid"),
                    Some(&credential),
                )
                .is_ok(),
                "all causes true for {dialect}/{adapter} -> E1"
            );
        }
        assert!(matches!(
            executor_from_materialized_endpoint(
                "",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedDialect(dialect)) if dialect.is_empty()
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "unknown",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedDialect(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "anthropic_messages",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::DialectAdapterMismatch { .. })
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "",
                "unknown",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedDialect(dialect)) if dialect.is_empty()
        ));
        assert!(matches!(
            executor_from_materialized_endpoint("open_ai_chat", "openai", None, Some(&credential)),
            Err(ResolvedExecutorError::MissingBaseUrl(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "open_ai_chat",
                "openai",
                Some("https://provider.invalid"),
                None,
            ),
            Err(ResolvedExecutorError::MissingCredential)
        ));
        // C6 the immutable publication selected Disabled for DeepSeek Chat;
        // C7 the same policy reaches a Responses adapter that has no faithful
        // projection. E3 Chat accepts the policy for the provider-wire gate;
        // E4 Responses fails before any network I/O. Rules R3=C1..C5+C6=>E3;
        // R4=C1..C5+C7=>E4. The HTTP-byte assertion lives in the genai
        // compatibility suite and prevents an accepted but dropped override.
        assert!(
            executor_from_materialized_endpoint_for_provider(
                "deepseek",
                "open_ai_chat",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
                UnspecifiedReasoning::Disabled,
            )
            .is_ok(),
            "R3"
        );
        assert!(
            matches!(
                executor_from_materialized_endpoint_for_provider(
                    "deepseek",
                    "open_ai_responses",
                    "openai",
                    Some("https://provider.invalid"),
                    Some(&credential),
                    UnspecifiedReasoning::Disabled,
                ),
                Err(ResolvedExecutorError::UnsupportedReasoningPolicy(_))
            ),
            "R4"
        );
    }
}
