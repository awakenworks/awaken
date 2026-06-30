//! The real model port for the coding agent: a `genai`-backed [`GenaiExecutor`].
//!
//! When `MINIMAX_API_KEY` is set, every model routes to MiniMax's
//! Anthropic-compatible endpoint; otherwise `genai`'s default client reads the
//! standard `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` from the environment.

use awaken_provider_genai::GenaiExecutor;

/// Build the executor from the environment (MiniMax when its key is present).
pub fn build_executor() -> anyhow::Result<GenaiExecutor> {
    match std::env::var("MINIMAX_API_KEY") {
        Ok(key) if !key.is_empty() => {
            let base = std::env::var("MINIMAX_BASE_URL")
                .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic".to_string());
            Ok(GenaiExecutor::with_client(minimax_client(key, base)?))
        }
        _ => Ok(GenaiExecutor::new()),
    }
}

/// A `genai` client that routes every model to the MiniMax Anthropic-compatible
/// endpoint with the given key.
fn minimax_client(api_key: String, base_url: String) -> anyhow::Result<genai::Client> {
    use genai::ModelIden;
    use genai::adapter::AdapterKind;
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
                model: ModelIden::new(AdapterKind::Anthropic, model.model_name),
            })
        },
    );

    Ok(Client::builder()
        .with_service_target_resolver(resolver)
        .build())
}
