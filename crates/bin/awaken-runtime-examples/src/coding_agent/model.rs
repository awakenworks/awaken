//! The real model port for the coding agent: a `genai`-backed [`GenaiExecutor`].
//!
//! Selection by environment, in order:
//! - `MINIMAX_API_KEY` → MiniMax's Anthropic-compatible endpoint
//!   (`MINIMAX_BASE_URL`, default `https://api.minimaxi.com/anthropic`).
//! - `KIMI_API_KEY` → Kimi Code's OpenAI-compatible endpoint
//!   (`KIMI_BASE_URL`, default `https://api.kimi.com/coding/v1`).
//! - otherwise `genai`'s default client (standard `OPENAI_API_KEY` /
//!   `ANTHROPIC_API_KEY` from the environment).

use std::sync::Arc;

use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use genai::adapter::AdapterKind;

/// Build the model port from the environment, as a non-streaming executor.
pub fn build_executor() -> anyhow::Result<Arc<dyn LlmExecutor>> {
    let genai = if let Some(key) = env_nonempty("MINIMAX_API_KEY") {
        let base = std::env::var("MINIMAX_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic".to_string());
        GenaiExecutor::with_client(custom_client(key, base, AdapterKind::Anthropic)?)
    } else if let Some(key) = env_nonempty("KIMI_API_KEY") {
        let base = std::env::var("KIMI_BASE_URL")
            .unwrap_or_else(|_| "https://api.kimi.com/coding/v1".to_string());
        GenaiExecutor::with_client(custom_client(key, base, AdapterKind::OpenAI)?)
    } else {
        GenaiExecutor::new()
    };
    Ok(Arc::new(NonStreaming(genai)))
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
    Ok(Client::builder()
        .with_service_target_resolver(resolver)
        .with_chat_options(ChatOptions::default().with_max_tokens(8192))
        .build())
}
