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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryAuthScheme {
    Bearer,
    GoogleApiKey,
    None,
}

impl DiscoveryAuthScheme {
    fn default_for_schema(schema: &str) -> Self {
        match schema {
            "gemini" => Self::GoogleApiKey,
            _ => Self::Bearer,
        }
    }
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
    if let Some(headers) = auth_headers(provider, schema) {
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

fn provider_discovery_auth_scheme(provider: &ProviderSpec, schema: &str) -> DiscoveryAuthScheme {
    let default = DiscoveryAuthScheme::default_for_schema(schema);
    let Some(declared) = provider
        .adapter_options
        .get("model_discovery_auth")
        .and_then(|value| value.as_str())
    else {
        return default;
    };
    match declared.to_ascii_lowercase().as_str() {
        "bearer" | "authorization-bearer" => DiscoveryAuthScheme::Bearer,
        "x-goog-api-key" | "google-api-key" | "gemini-api-key" => DiscoveryAuthScheme::GoogleApiKey,
        "none" | "no-auth" | "disabled" => DiscoveryAuthScheme::None,
        other => {
            tracing::warn!(
                provider_id = %provider.id,
                model_discovery_auth = other,
                "ignoring unknown adapter_options.model_discovery_auth"
            );
            default
        }
    }
}

fn auth_headers(provider: &ProviderSpec, schema: &str) -> Option<HeaderMap> {
    let scheme = provider_discovery_auth_scheme(provider, schema);
    if scheme == DiscoveryAuthScheme::None {
        return None;
    }
    let api_key = provider
        .api_key
        .as_ref()
        .map(|key| key.expose_secret().trim())
        .filter(|key| !key.is_empty())?;
    let mut headers = HeaderMap::new();
    match scheme {
        DiscoveryAuthScheme::GoogleApiKey => {
            headers.insert("x-goog-api-key", HeaderValue::from_str(api_key).ok()?);
        }
        DiscoveryAuthScheme::Bearer => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}")).ok()?,
            );
        }
        DiscoveryAuthScheme::None => return None,
    }
    Some(headers)
}

#[cfg(test)]
#[path = "provider_capability_discovery_tests.rs"]
mod tests;
