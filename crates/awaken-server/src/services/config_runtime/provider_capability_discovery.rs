use std::collections::{HashMap, HashSet};
use std::time::Duration;

use awaken_contract::{ModelPoolSpec, ModelSpec, ProviderSpec};
use awaken_runtime::registry::model_capabilities::{
    ModelCapabilityPatch, normalize_capability_model_name, parse_provider_model_capabilities,
};
use futures::future::join_all;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

/// Result of a provider-capability discovery pass.
///
/// `attempted` distinguishes providers we actually issued a discovery request
/// for (whether it then succeeded or failed) from providers we deliberately did
/// not probe this round (no referenced model needs discovery, or the default
/// endpoint was skipped for lack of credentials). The capability cache uses it
/// to warn about *stale* snapshots only when discovery was attempted and failed,
/// never when discovery was simply unnecessary.
#[derive(Default)]
pub(super) struct ProviderCapabilityDiscovery {
    pub(super) discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
    pub(super) attempted: HashSet<String>,
}

enum DiscoveryOutcome {
    /// Discovery was not issued (skipped endpoint or no resolvable model URL).
    NotAttempted,
    /// A discovery request was issued but did not yield usable metadata.
    Failed,
    /// A discovery request succeeded; the map may be empty.
    Discovered(HashMap<String, ModelCapabilityPatch>),
}

pub(super) async fn discover_provider_capabilities(
    providers: &[ProviderSpec],
    models: &[ModelSpec],
    pools: &[ModelPoolSpec],
) -> ProviderCapabilityDiscovery {
    let wanted = referenced_models_by_provider(providers, models, pools);
    if wanted.is_empty() {
        return ProviderCapabilityDiscovery::default();
    }

    let client = reqwest::Client::new();
    let tasks = providers
        .iter()
        .filter(|provider| wanted.contains_key(&provider.id))
        .map(|provider| {
            let client = client.clone();
            let wanted = wanted.get(&provider.id).cloned().unwrap_or_default();
            async move {
                let outcome = discover_one_provider(&client, provider, &wanted).await;
                (provider.id.clone(), outcome)
            }
        });

    let mut result = ProviderCapabilityDiscovery::default();
    for (provider_id, outcome) in join_all(tasks).await {
        match outcome {
            DiscoveryOutcome::NotAttempted => {}
            DiscoveryOutcome::Failed => {
                result.attempted.insert(provider_id);
            }
            DiscoveryOutcome::Discovered(capabilities) => {
                result.attempted.insert(provider_id.clone());
                result.discovered.insert(provider_id, capabilities);
            }
        }
    }
    result
}

async fn discover_one_provider(
    client: &reqwest::Client,
    provider: &ProviderSpec,
    wanted: &HashSet<String>,
) -> DiscoveryOutcome {
    // Only providers with a known `/models` schema are probed. An unknown or
    // custom adapter is NOT assumed to be OpenAI-compatible — its endpoint
    // would otherwise be parsed as trusted OpenAI metadata. Custom providers
    // opt in explicitly via `adapter_options.model_discovery_schema`.
    let Some(schema) = provider_discovery_schema(provider) else {
        tracing::debug!(
            provider_id = %provider.id,
            adapter = %provider.adapter,
            "skipping model capability discovery: adapter has no known /models schema \
             (set adapter_options.model_discovery_schema to opt in)"
        );
        return DiscoveryOutcome::NotAttempted;
    };
    if should_skip_unauthenticated_default_endpoint(provider) {
        tracing::debug!(
            provider_id = %provider.id,
            adapter = %provider.adapter,
            "skipping provider model capability discovery without explicit credentials"
        );
        return DiscoveryOutcome::NotAttempted;
    }
    let url = match model_list_url(provider) {
        Some(url) => url,
        None => return DiscoveryOutcome::NotAttempted,
    };
    let mut request = client
        .get(url.clone())
        .timeout(Duration::from_secs(provider.timeout_secs.clamp(1, 30)));
    if let Some(headers) = auth_headers(provider) {
        request = request.headers(headers);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                provider_id = %provider.id,
                adapter = %provider.adapter,
                url = %url,
                error = %error,
                "failed to discover provider model capabilities"
            );
            return DiscoveryOutcome::Failed;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            provider_id = %provider.id,
            adapter = %provider.adapter,
            url = %url,
            status = %response.status(),
            "provider model capability discovery returned non-success status"
        );
        return DiscoveryOutcome::Failed;
    }

    let payload = match response.json::<serde_json::Value>().await {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                provider_id = %provider.id,
                adapter = %provider.adapter,
                url = %url,
                error = %error,
                "provider model capability discovery returned invalid json"
            );
            return DiscoveryOutcome::Failed;
        }
    };
    let parsed = parse_provider_model_capabilities(schema, &payload);
    if !parsed.keys().any(|model| wanted.contains(model)) {
        tracing::debug!(
            provider_id = %provider.id,
            adapter = %provider.adapter,
            "provider model capability discovery succeeded without wanted model metadata"
        );
    }
    DiscoveryOutcome::Discovered(parsed)
}

fn referenced_models_by_provider(
    providers: &[ProviderSpec],
    models: &[ModelSpec],
    pools: &[ModelPoolSpec],
) -> HashMap<String, HashSet<String>> {
    let schema_by_provider: HashMap<&str, &'static str> = providers
        .iter()
        .filter_map(|provider| {
            provider_discovery_schema(provider).map(|schema| (provider.id.as_str(), schema))
        })
        .collect();
    let models_by_id: HashMap<_, _> = models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect();
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();

    let consider = |model: &ModelSpec, out: &mut HashMap<String, HashSet<String>>| {
        let Some(schema) = schema_by_provider.get(model.provider_id.as_str()) else {
            return;
        };
        if needs_capability_discovery(model, schema) {
            out.entry(model.provider_id.clone())
                .or_default()
                .insert(normalize_capability_model_name(&model.upstream_model));
        }
    };

    for model in models {
        consider(model, &mut out);
    }
    for pool in pools {
        for member in &pool.members {
            let Some(model) = models_by_id.get(member.model_id.as_str()) else {
                continue;
            };
            consider(model, &mut out);
        }
    }

    out
}

/// Whether a model still has a capability field that *this provider's discovery
/// schema can fill*. Token limits are discoverable on every schema, but only the
/// OpenAI-compatible schema surfaces modalities and knowledge cutoff — so a
/// Gemini-backed model missing only those fields must not keep re-triggering a
/// probe on every publish (the probe could never fill them).
fn needs_capability_discovery(model: &ModelSpec, schema: &str) -> bool {
    let token_limits_missing = model.context_window.is_none() || model.max_output_tokens.is_none();
    let modalities_missing =
        model.modalities.input.is_empty() || model.modalities.output.is_empty();
    let cutoff_missing = model.knowledge_cutoff.is_none();
    match schema {
        "openai" => token_limits_missing || modalities_missing || cutoff_missing,
        "gemini" => token_limits_missing,
        _ => false,
    }
}

fn model_list_url(provider: &ProviderSpec) -> Option<reqwest::Url> {
    let base = provider
        .base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| default_model_base_url(&provider.adapter))?;
    let trimmed = base.trim();
    if base_url_looks_like_inference_endpoint(trimmed) {
        tracing::warn!(
            provider_id = %provider.id,
            base_url = trimmed,
            "skipping provider model discovery because base_url is not an API root"
        );
        return None;
    }
    if trimmed.ends_with("/models") || trimmed.ends_with("/models/") {
        return reqwest::Url::parse(trimmed).ok();
    }
    let base = if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    };
    reqwest::Url::parse(&base).ok()?.join("models").ok()
}

fn base_url_looks_like_inference_endpoint(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    let path = url.path().trim_end_matches('/');
    path.ends_with("/chat/completions")
        || path.ends_with("/completions")
        || path.ends_with("/responses")
        || path.ends_with(":generateContent")
        || path.ends_with(":streamGenerateContent")
}

fn default_model_base_url(adapter: &str) -> Option<&'static str> {
    match adapter.to_ascii_lowercase().as_str() {
        "openai" => Some("https://api.openai.com/v1/"),
        "openrouter" => Some("https://openrouter.ai/api/v1/"),
        "gemini" | "google" => Some("https://generativelanguage.googleapis.com/v1beta/"),
        _ => None,
    }
}

/// Resolve the `/models` discovery schema for a provider, or `None` to skip
/// discovery entirely.
///
/// Built-in adapters map to their native schema. Any other adapter must opt in
/// explicitly via `adapter_options.model_discovery_schema` (`"openai"` /
/// `"openai-compatible"` or `"gemini"`) — so a custom OpenAI-compatible gateway
/// can be discovered while an unknown adapter is never silently trusted as
/// OpenAI metadata. The returned string is the `provider_source` passed to
/// [`parse_provider_model_capabilities`].
fn provider_discovery_schema(provider: &ProviderSpec) -> Option<&'static str> {
    if let Some(declared) = provider
        .adapter_options
        .get("model_discovery_schema")
        .and_then(|value| value.as_str())
    {
        return match declared.to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openrouter" => Some("openai"),
            "gemini" | "google" => Some("gemini"),
            other => {
                tracing::warn!(
                    provider_id = %provider.id,
                    model_discovery_schema = other,
                    "ignoring unknown adapter_options.model_discovery_schema"
                );
                None
            }
        };
    }
    match provider.adapter.to_ascii_lowercase().as_str() {
        "openai" | "openrouter" => Some("openai"),
        "gemini" | "google" => Some("gemini"),
        _ => None,
    }
}

fn should_skip_unauthenticated_default_endpoint(provider: &ProviderSpec) -> bool {
    if provider.base_url.is_some() || provider.api_key.is_some() {
        return false;
    }
    matches!(
        provider.adapter.to_ascii_lowercase().as_str(),
        "openai" | "gemini" | "google"
    )
}

fn auth_headers(provider: &ProviderSpec) -> Option<HeaderMap> {
    let api_key = provider
        .api_key
        .as_ref()
        .map(|key| key.expose_secret().trim())
        .filter(|key| !key.is_empty())?;
    let mut headers = HeaderMap::new();
    match provider.adapter.to_ascii_lowercase().as_str() {
        "gemini" | "google" => {
            headers.insert("x-goog-api-key", HeaderValue::from_str(api_key).ok()?);
        }
        _ => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}")).ok()?,
            );
        }
    }
    Some(headers)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap as AxumHeaderMap, StatusCode};
    use axum::response::Json;
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn model_list_url_appends_models_to_base_path() {
        let provider = ProviderSpec {
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };

        assert_eq!(
            model_list_url(&provider).unwrap().as_str(),
            "https://example.test/v1/models"
        );
    }

    #[test]
    fn model_list_url_rejects_inference_endpoint_base_url() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1/chat/completions".into()),
            ..ProviderSpec::default()
        };

        assert!(model_list_url(&provider).is_none());
    }

    #[test]
    fn default_openai_discovery_requires_explicit_credentials() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            ..ProviderSpec::default()
        };

        assert!(should_skip_unauthenticated_default_endpoint(&provider));
    }

    #[test]
    fn vertex_discovery_has_no_implicit_endpoint() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "vertex".into(),
            ..ProviderSpec::default()
        };

        assert!(model_list_url(&provider).is_none());
    }

    #[test]
    fn referenced_models_include_pool_members_once() {
        let models = vec![
            ModelSpec::new("m0", "p", "gpt-4o"),
            ModelSpec {
                context_window: Some(10),
                max_output_tokens: Some(10),
                modalities: awaken_contract::registry_spec::Modalities {
                    input: vec![awaken_contract::registry_spec::Modality::Text],
                    output: vec![awaken_contract::registry_spec::Modality::Text],
                },
                knowledge_cutoff: Some("2025-01".into()),
                ..ModelSpec::new("m1", "p", "complete")
            },
        ];
        let pools = vec![ModelPoolSpec {
            id: "pool".into(),
            members: vec![awaken_contract::registry_spec::PoolMemberSpec {
                model_id: "m0".into(),
                role: awaken_contract::registry_spec::PoolMemberRole::Member,
                weight: None,
            }],
            routing: Default::default(),
            switch: Default::default(),
        }];
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            ..ProviderSpec::default()
        }];

        let wanted = referenced_models_by_provider(&providers, &models, &pools);

        assert_eq!(wanted["p"].len(), 1);
        assert!(wanted["p"].contains("gpt-4o"));
    }

    #[test]
    fn gemini_model_missing_only_cutoff_is_not_requested() {
        // Gemini discovery cannot fill modalities or knowledge cutoff, so a
        // model that already has token limits must not keep re-triggering a
        // probe just because those fields are absent.
        let providers = vec![ProviderSpec {
            id: "g".into(),
            adapter: "gemini".into(),
            ..ProviderSpec::default()
        }];
        let models = vec![ModelSpec {
            context_window: Some(1_000_000),
            max_output_tokens: Some(8_192),
            ..ModelSpec::new("m", "g", "gemini-2.5-pro")
        }];

        let wanted = referenced_models_by_provider(&providers, &models, &[]);

        assert!(
            wanted.is_empty(),
            "token limits present and Gemini cannot fill modalities/cutoff: no probe"
        );
    }

    #[test]
    fn gemini_model_missing_token_limits_is_requested() {
        // Token limits are discoverable on Gemini, so a missing one still drives
        // a probe.
        let providers = vec![ProviderSpec {
            id: "g".into(),
            adapter: "gemini".into(),
            ..ProviderSpec::default()
        }];
        let models = vec![ModelSpec::new("m", "g", "gemini-2.5-pro")];

        let wanted = referenced_models_by_provider(&providers, &models, &[]);

        assert!(wanted.contains_key("g"));
    }

    #[tokio::test]
    async fn discovers_openai_compatible_capabilities_from_models_endpoint() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base_url = spawn_models_server(Arc::clone(&hits)).await;
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "openrouter".into(),
            api_key: Some("secret".into()),
            base_url: Some(base_url),
            timeout_secs: 5,
            adapter_options: Default::default(),
        }];
        let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

        let result = discover_provider_capabilities(&providers, &models, &[]).await;

        let patch = result.discovered["p"].get("openai/gpt-4o").expect("patch");
        assert_eq!(patch.context_window, Some(128_000));
        assert_eq!(patch.max_output_tokens, Some(16_384));
        assert!(result.attempted.contains("p"), "p was probed");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_discovery_without_wanted_models_returns_full_snapshot() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base_url = spawn_models_server(Arc::clone(&hits)).await;
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "openrouter".into(),
            api_key: Some("secret".into()),
            base_url: Some(base_url),
            timeout_secs: 5,
            adapter_options: Default::default(),
        }];
        let models = vec![ModelSpec::new("m", "p", "missing-model")];

        let result = discover_provider_capabilities(&providers, &models, &[]).await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            result.discovered.get("p"),
            Some(&HashMap::from([(
                "openai/gpt-4o".to_string(),
                ModelCapabilityPatch {
                    context_window: Some(128_000),
                    max_output_tokens: Some(16_384),
                    modalities: None,
                    knowledge_cutoff: None,
                }
            )]))
        );
    }

    #[tokio::test]
    async fn fully_specified_models_are_not_attempted() {
        // Every model capability is explicit, so no provider needs discovery:
        // nothing is attempted and the stale-snapshot warning cannot fire.
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "openrouter".into(),
            api_key: Some("secret".into()),
            base_url: Some("https://example.test/v1".into()),
            timeout_secs: 5,
            adapter_options: Default::default(),
        }];
        let models = vec![ModelSpec {
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            modalities: awaken_contract::registry_spec::Modalities {
                input: vec![awaken_contract::registry_spec::Modality::Text],
                output: vec![awaken_contract::registry_spec::Modality::Text],
            },
            knowledge_cutoff: Some("2025-01".into()),
            ..ModelSpec::new("m", "p", "gpt-4o")
        }];

        let result = discover_provider_capabilities(&providers, &models, &[]).await;

        assert!(result.discovered.is_empty());
        assert!(
            result.attempted.is_empty(),
            "no discovery was needed, so no provider is attempted"
        );
    }

    #[tokio::test]
    async fn unknown_adapter_is_not_probed_without_opt_in() {
        // An unknown adapter with an explicit base_url must NOT be probed and
        // parsed as trusted OpenAI metadata.
        let hits = Arc::new(AtomicUsize::new(0));
        let base_url = spawn_models_server(Arc::clone(&hits)).await;
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "custom-gateway".into(),
            api_key: Some("secret".into()),
            base_url: Some(base_url),
            timeout_secs: 5,
            adapter_options: Default::default(),
        }];
        let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

        let result = discover_provider_capabilities(&providers, &models, &[]).await;

        assert!(result.discovered.is_empty());
        assert!(result.attempted.is_empty());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "unknown adapter must not be probed"
        );
    }

    #[tokio::test]
    async fn custom_adapter_is_probed_with_explicit_schema_opt_in() {
        // A custom OpenAI-compatible gateway opts in via adapter_options.
        let hits = Arc::new(AtomicUsize::new(0));
        let base_url = spawn_models_server(Arc::clone(&hits)).await;
        let providers = vec![ProviderSpec {
            id: "p".into(),
            adapter: "custom-gateway".into(),
            api_key: Some("secret".into()),
            base_url: Some(base_url),
            timeout_secs: 5,
            adapter_options: [(
                "model_discovery_schema".to_string(),
                json!("openai-compatible"),
            )]
            .into_iter()
            .collect(),
        }];
        let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

        let result = discover_provider_capabilities(&providers, &models, &[]).await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an opted-in custom adapter is probed"
        );
        let patch = result.discovered["p"].get("openai/gpt-4o").expect("patch");
        assert_eq!(patch.context_window, Some(128_000));
        assert!(result.attempted.contains("p"));
    }

    async fn spawn_models_server(hits: Arc<AtomicUsize>) -> String {
        async fn handler(
            State(hits): State<Arc<AtomicUsize>>,
            headers: AxumHeaderMap,
        ) -> Result<Json<serde_json::Value>, StatusCode> {
            let Some(auth) = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
            else {
                return Err(StatusCode::UNAUTHORIZED);
            };
            if auth != "Bearer secret" {
                return Err(StatusCode::UNAUTHORIZED);
            }
            hits.fetch_add(1, Ordering::SeqCst);
            Ok(Json(json!({
                "data": [{
                    "id": "openai/gpt-4o",
                    "context_length": 128000,
                    "top_provider": { "max_completion_tokens": 16384 }
                }]
            })))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        let app = Router::new()
            .route("/v1/models", get(handler))
            .with_state(hits);
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}/v1")
    }
}
