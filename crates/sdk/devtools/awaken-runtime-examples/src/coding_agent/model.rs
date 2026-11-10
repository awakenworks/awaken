//! The real model port for the coding agent: a `genai`-backed [`GenaiExecutor`].
//!
//! Selection by environment, in order:
//! - `MINIMAX_API_KEY` → MiniMax's Anthropic-compatible endpoint
//!   (`MINIMAX_BASE_URL`, default `https://api.minimaxi.com/anthropic`).
//! - `KIMI_API_KEY` → Kimi Code's OpenAI-compatible endpoint
//!   (`KIMI_BASE_URL`, default `https://api.kimi.com/coding/v1`).
//! - `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` → an explicitly selected native
//!   adapter. Missing credentials fail closed; the SDK ambient default is unused.

use std::sync::Arc;

use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use genai::adapter::AdapterKind;

/// Select the model and its non-streaming executor from one explicit environment.
pub fn build_executor() -> anyhow::Result<(Arc<dyn LlmExecutor>, String)> {
    let configuration = model_configuration(env_nonempty)?;
    let genai = GenaiExecutor::with_client_for_adapter(
        custom_client(configuration.key, configuration.base, configuration.adapter)?,
        configuration.adapter,
    );
    Ok((Arc::new(NonStreaming(genai)), configuration.model))
}

struct ModelConfiguration {
    adapter: AdapterKind,
    base: String,
    key: String,
    model: String,
}

// One provider decision supplies both the executor and its model. The TUI and
// live examples cannot independently choose incompatible defaults.
fn model_configuration(
    read: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<ModelConfiguration> {
    let providers = [
        (
            "MINIMAX_API_KEY",
            "MINIMAX_BASE_URL",
            "https://api.minimaxi.com/anthropic",
            AdapterKind::Anthropic,
            Some("MiniMax-M3"),
        ),
        (
            "KIMI_API_KEY",
            "KIMI_BASE_URL",
            "https://api.kimi.com/coding/v1",
            AdapterKind::OpenAI,
            Some("kimi-k2.7-code"),
        ),
        (
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "https://api.openai.com/v1",
            AdapterKind::OpenAI,
            None,
        ),
        (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "https://api.anthropic.com/v1",
            AdapterKind::Anthropic,
            None,
        ),
    ];
    for (key_name, base_name, default_base, adapter, default_model) in providers {
        if let Some(key) = read(key_name) {
            let model = read("AWAKEN_MODEL")
                .or_else(|| default_model.map(str::to_owned))
                .ok_or_else(|| {
                    anyhow::anyhow!("set AWAKEN_MODEL to a model offered by the selected endpoint")
                })?;
            return Ok(ModelConfiguration {
                adapter,
                key,
                model,
                base: read(base_name).unwrap_or_else(|| default_base.to_owned()),
            });
        }
    }
    anyhow::bail!("set an explicit provider credential for the coding-agent example")
}

/// Forces non-streaming inference: it delegates `infer` and lets the trait's
/// default `infer_streaming` emit the assembled turn. genai's streaming capture
/// drops tool calls for some reasoning models (e.g. Kimi K2 emits a long
/// `reasoning_content` stream before the call), so the agent uses plain inference.
struct NonStreaming(GenaiExecutor);

#[async_trait::async_trait]
impl LlmExecutor for NonStreaming {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.0.infer(request).await
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the example program is its own process root and reads its model endpoint from the environment"
)]
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// A `genai` client that routes every model to one OpenAI- or Anthropic-compatible
/// endpoint with the given key — the pattern for any compatible gateway.
fn custom_client(
    api_key: String,
    base_url: String,
    adapter: AdapterKind,
) -> anyhow::Result<genai::Client> {
    use genai::ModelIden;
    use genai::chat::ChatOptions;
    use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
    use genai::{Client, ServiceTarget};

    let base_url = if base_url.ends_with('/') {
        base_url
    } else {
        format!("{base_url}/")
    };

    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let ServiceTarget { model, .. } = target;
            Ok(ServiceTarget {
                endpoint: Endpoint::from_owned(base_url.clone()),
                auth: AuthData::from_single(api_key.clone()),
                model: ModelIden::new(adapter, model.model_name),
            })
        },
    );

    // A generous default token budget: reasoning models spend tokens thinking
    // before they emit a tool call, and the provider sends no per-call limit.
    Client::builder()
        .with_service_target_resolver(resolver)
        .with_chat_options(ChatOptions::default().with_max_tokens(8192))
        .build()
        .map_err(Into::into)
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn provider_and_model_are_selected_together() {
        // Causes: C1 available credentials in their documented priority; C2
        // explicit model or named-provider default; C3 custom endpoint; C4 no
        // credential. Effects: E1 one matching model/adapter; E2 explicit model
        // wins; E3 custom URL retained; E4 missing model/credential refuses.
        // Rules: T1 MiniMax+Kimi -> MiniMax/default; T2 Kimi -> Kimi/default;
        // T3 OpenAI+explicit model+URL -> E1/E2/E3; T4 native key without model
        // or no key -> E4. Pure inputs avoid process-global environment mutation.
        let select = |pairs: &[(&str, &str)]| {
            model_configuration(|name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        let named = select(&[("MINIMAX_API_KEY", "fixture"), ("KIMI_API_KEY", "fixture")]).unwrap();
        assert_eq!(named.model, "MiniMax-M3");
        assert!(matches!(named.adapter, AdapterKind::Anthropic));
        assert_eq!(
            select(&[("KIMI_API_KEY", "fixture")]).unwrap().model,
            "kimi-k2.7-code"
        );
        let custom = select(&[
            ("OPENAI_API_KEY", "fixture"),
            ("OPENAI_BASE_URL", "https://gateway.example/v1"),
            ("AWAKEN_MODEL", "selected-model"),
        ])
        .unwrap();
        assert_eq!(custom.model, "selected-model");
        assert_eq!(custom.base, "https://gateway.example/v1");
        assert!(select(&[("OPENAI_API_KEY", "fixture")]).is_err());
        assert!(select(&[]).is_err());
    }
}
