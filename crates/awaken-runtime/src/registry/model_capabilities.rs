//! Model capability defaults and provider-discovered capability overlays.
//!
//! The serialized `ModelSpec` remains authoritative. Resolver backfill only
//! fills omitted capability fields, preferring provider `/models` discoveries
//! when present and falling back to conservative built-in defaults.

use std::collections::HashMap;

use awaken_contract::registry_spec::{Modalities, Modality, ModelSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySource {
    ExplicitSpec,
    ProviderDiscovery,
    StaticHeuristic,
}

impl CapabilitySource {
    pub fn is_runtime_trusted(self) -> bool {
        matches!(self, Self::ExplicitSpec | Self::ProviderDiscovery)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilitySources {
    pub context_window: Option<CapabilitySource>,
    pub max_output_tokens: Option<CapabilitySource>,
    pub modalities: Option<CapabilitySource>,
    pub knowledge_cutoff: Option<CapabilitySource>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelCapabilities {
    pub model: ModelSpec,
    pub sources: ModelCapabilitySources,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilityPatch {
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub modalities: Option<Modalities>,
    pub knowledge_cutoff: Option<String>,
}

impl ModelCapabilityPatch {
    fn vision(context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            modalities: Some(vision_modalities()),
            knowledge_cutoff: None,
        }
    }

    fn multimodal(context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            modalities: Some(multimodal_modalities()),
            knowledge_cutoff: None,
        }
    }

    fn vision_with_cutoff(
        context_window: u32,
        max_output_tokens: u32,
        knowledge_cutoff: impl Into<String>,
    ) -> Self {
        Self {
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            modalities: Some(vision_modalities()),
            knowledge_cutoff: Some(knowledge_cutoff.into()),
        }
    }
}

#[cfg(test)]
fn backfill_model_capabilities(
    model: ModelSpec,
    provider_source: Option<&str>,
    discovered: Option<&ModelCapabilityPatch>,
) -> ModelSpec {
    resolve_model_capabilities(model, provider_source, discovered).model
}

pub(crate) fn resolve_model_capabilities(
    mut model: ModelSpec,
    provider_source: Option<&str>,
    discovered: Option<&ModelCapabilityPatch>,
) -> ResolvedModelCapabilities {
    let mut sources = ModelCapabilitySources {
        context_window: model.context_window.map(|_| CapabilitySource::ExplicitSpec),
        max_output_tokens: model
            .max_output_tokens
            .map(|_| CapabilitySource::ExplicitSpec),
        modalities: (!model.modalities.input.is_empty() || !model.modalities.output.is_empty())
            .then_some(CapabilitySource::ExplicitSpec),
        knowledge_cutoff: model
            .knowledge_cutoff
            .as_ref()
            .map(|_| CapabilitySource::ExplicitSpec),
    };

    let defaults = discovered
        .cloned()
        .map(|patch| (patch, CapabilitySource::ProviderDiscovery))
        .or_else(|| {
            provider_source
                .and_then(|source| lookup(source, &model.upstream_model))
                .or_else(|| lookup(&model.provider_id, &model.upstream_model))
                .map(|patch| (patch, CapabilitySource::StaticHeuristic))
        });

    let Some((defaults, source)) = defaults else {
        return ResolvedModelCapabilities { model, sources };
    };

    if model.context_window.is_none() {
        model.context_window = defaults.context_window;
        if model.context_window.is_some() {
            sources.context_window = Some(source);
        }
    }
    if model.max_output_tokens.is_none() {
        model.max_output_tokens = defaults.max_output_tokens;
        if model.max_output_tokens.is_some() {
            sources.max_output_tokens = Some(source);
        }
    }
    if model.modalities.input.is_empty()
        && model.modalities.output.is_empty()
        && let Some(modalities) = defaults.modalities
    {
        model.modalities = modalities;
        sources.modalities = Some(source);
    }
    if model.knowledge_cutoff.is_none() {
        model.knowledge_cutoff = defaults.knowledge_cutoff;
        if model.knowledge_cutoff.is_some() {
            sources.knowledge_cutoff = Some(source);
        }
    }

    ResolvedModelCapabilities { model, sources }
}

pub fn normalize_capability_model_name(value: &str) -> String {
    value
        .trim()
        .strip_prefix("models/")
        .unwrap_or_else(|| value.trim())
        .to_ascii_lowercase()
}

pub fn parse_provider_model_capabilities(
    provider_source: &str,
    payload: &serde_json::Value,
) -> HashMap<String, ModelCapabilityPatch> {
    match provider_source.to_ascii_lowercase().as_str() {
        "gemini" | "google" | "vertex" => parse_gemini_model_capabilities(payload),
        _ => parse_openai_compatible_model_capabilities(payload),
    }
}

fn lookup(provider: &str, upstream_model: &str) -> Option<ModelCapabilityPatch> {
    let provider = provider.to_ascii_lowercase();
    let model = normalize_capability_model_name(upstream_model);
    match provider.as_str() {
        "openai" => openai_defaults(&model),
        "anthropic" => anthropic_defaults(&model),
        "gemini" | "google" | "vertex" => gemini_defaults(&model),
        "openrouter" => openrouter_defaults(&model),
        _ => None,
    }
}

fn openrouter_defaults(model: &str) -> Option<ModelCapabilityPatch> {
    let (_, routed) = model.split_once('/')?;
    openai_defaults(routed)
        .or_else(|| anthropic_defaults(routed))
        .or_else(|| gemini_defaults(routed))
}

fn openai_defaults(model: &str) -> Option<ModelCapabilityPatch> {
    if model == "gpt-5.5" || model == "gpt-5.5-pro" {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            1_050_000,
            128_000,
            "2025-12-01",
        ));
    }
    if model == "gpt-5.4" {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            1_050_000,
            128_000,
            "2025-08-31",
        ));
    }
    if model == "gpt-4o" || model.starts_with("gpt-4o-") {
        return Some(ModelCapabilityPatch::vision(128_000, 16_384));
    }
    if model == "gpt-4o-mini" || model.starts_with("gpt-4o-mini-") {
        return Some(ModelCapabilityPatch::vision(128_000, 16_384));
    }
    if model == "gpt-4.1"
        || model.starts_with("gpt-4.1-")
        || model == "gpt-4.1-mini"
        || model.starts_with("gpt-4.1-mini-")
        || model == "gpt-4.1-nano"
        || model.starts_with("gpt-4.1-nano-")
    {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            1_047_576,
            32_768,
            "2024-06-01",
        ));
    }
    if model == "o3" || model.starts_with("o3-") || model == "o4-mini" {
        return Some(ModelCapabilityPatch::vision(200_000, 100_000));
    }
    if model == "o1" || model.starts_with("o1-") {
        return Some(ModelCapabilityPatch::vision(200_000, 100_000));
    }
    None
}

fn anthropic_defaults(model: &str) -> Option<ModelCapabilityPatch> {
    if model == "claude-opus-4-7" {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            1_000_000, 128_000, "2026-01",
        ));
    }
    if model == "claude-sonnet-4-6" {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            1_000_000, 64_000, "2025-08",
        ));
    }
    if model.starts_with("claude-haiku-4-5") {
        return Some(ModelCapabilityPatch::vision_with_cutoff(
            200_000, 64_000, "2025-02",
        ));
    }
    if model.starts_with("claude-opus-4-") || model.starts_with("claude-sonnet-4-") {
        return Some(ModelCapabilityPatch::vision(200_000, 32_000));
    }
    if model.starts_with("claude-3-")
        || model.starts_with("claude-opus-3-")
        || model.starts_with("claude-sonnet-3-")
        || model.starts_with("claude-haiku-3-")
    {
        return Some(ModelCapabilityPatch::vision(200_000, 8_192));
    }
    None
}

fn gemini_defaults(model: &str) -> Option<ModelCapabilityPatch> {
    if model.starts_with("gemini-1.5-") || model.starts_with("gemini-2.0-") {
        return Some(ModelCapabilityPatch::multimodal(1_048_576, 8_192));
    }
    if model.starts_with("gemini-2.5-") {
        return Some(ModelCapabilityPatch::multimodal(1_048_576, 65_536));
    }
    None
}

fn vision_modalities() -> Modalities {
    Modalities {
        input: vec![Modality::Text, Modality::Image],
        output: vec![Modality::Text],
    }
}

fn multimodal_modalities() -> Modalities {
    Modalities {
        input: vec![
            Modality::Text,
            Modality::Image,
            Modality::Audio,
            Modality::Video,
            Modality::Pdf,
        ],
        output: vec![Modality::Text],
    }
}

fn parse_openai_compatible_model_capabilities(
    payload: &serde_json::Value,
) -> HashMap<String, ModelCapabilityPatch> {
    let mut out = HashMap::new();
    let Some(models) = payload.get("data").and_then(|value| value.as_array()) else {
        return out;
    };

    for item in models {
        let Some(id) = item.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let patch = ModelCapabilityPatch {
            context_window: first_u32(item, &["context_window", "context_length", "context_size"]),
            max_output_tokens: first_u32(item, &["max_output_tokens", "max_completion_tokens"])
                .or_else(|| {
                    item.get("top_provider")
                        .and_then(|top| first_u32(top, &["max_completion_tokens"]))
                }),
            modalities: parse_openai_modalities(item),
            knowledge_cutoff: item
                .get("knowledge_cutoff")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
        };
        if patch.context_window.is_some()
            || patch.max_output_tokens.is_some()
            || patch.modalities.is_some()
            || patch.knowledge_cutoff.is_some()
        {
            out.insert(normalize_capability_model_name(id), patch);
        }
    }

    out
}

fn parse_gemini_model_capabilities(
    payload: &serde_json::Value,
) -> HashMap<String, ModelCapabilityPatch> {
    let mut out = HashMap::new();
    let Some(models) = payload.get("models").and_then(|value| value.as_array()) else {
        return out;
    };

    for item in models {
        let Some(name) = item.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let patch = ModelCapabilityPatch {
            context_window: first_u32(item, &["inputTokenLimit", "input_token_limit"]),
            max_output_tokens: first_u32(item, &["outputTokenLimit", "output_token_limit"]),
            modalities: None,
            knowledge_cutoff: None,
        };
        if patch.context_window.is_some() || patch.max_output_tokens.is_some() {
            out.insert(normalize_capability_model_name(name), patch);
        }
    }

    out
}

fn parse_openai_modalities(item: &serde_json::Value) -> Option<Modalities> {
    let architecture = item.get("architecture").unwrap_or(item);
    let input = parse_modality_array(
        architecture
            .get("input_modalities")
            .or_else(|| architecture.get("inputModalities")),
    );
    let output = parse_modality_array(
        architecture
            .get("output_modalities")
            .or_else(|| architecture.get("outputModalities")),
    );

    if input.is_empty() && output.is_empty() {
        None
    } else {
        Some(Modalities { input, output })
    }
}

fn parse_modality_array(value: Option<&serde_json::Value>) -> Vec<Modality> {
    let Some(values) = value.and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(|value| modality_from_str(value.as_str()?))
        .collect()
}

fn modality_from_str(value: &str) -> Option<Modality> {
    match value.trim().to_ascii_lowercase().as_str() {
        "text" => Some(Modality::Text),
        "image" | "images" => Some(Modality::Image),
        "audio" => Some(Modality::Audio),
        "video" => Some(Modality::Video),
        "pdf" | "document" | "documents" => Some(Modality::Pdf),
        _ => None,
    }
}

fn first_u32(item: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| item.get(*key).and_then(json_u32))
}

fn json_u32(value: &serde_json::Value) -> Option<u32> {
    if let Some(number) = value.as_u64() {
        return u32::try_from(number).ok();
    }
    value.as_str().and_then(|string| string.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_missing_openai_capabilities() {
        let resolved = resolve_model_capabilities(
            ModelSpec::new("m", "openai", "gpt-4o"),
            Some("openai"),
            None,
        );
        let model = resolved.model;

        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.max_output_tokens, Some(16_384));
        assert_eq!(model.modalities, vision_modalities());
        assert_eq!(
            resolved.sources.modalities,
            Some(CapabilitySource::StaticHeuristic)
        );
    }

    #[test]
    fn preserves_explicit_model_capabilities() {
        let explicit = ModelSpec {
            context_window: Some(32_000),
            max_output_tokens: Some(4_096),
            modalities: Modalities {
                input: vec![Modality::Text],
                output: vec![Modality::Text],
            },
            knowledge_cutoff: Some("2025-01".into()),
            ..ModelSpec::new("m", "openai", "gpt-4o")
        };

        assert_eq!(
            backfill_model_capabilities(explicit.clone(), Some("openai"), None),
            explicit
        );
    }

    #[test]
    fn provider_source_handles_provider_aliases() {
        let model = backfill_model_capabilities(
            ModelSpec::new("m", "prod-openai", "gpt-4o-mini"),
            Some("openai"),
            None,
        );

        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.max_output_tokens, Some(16_384));
    }

    #[test]
    fn fills_known_knowledge_cutoff_when_available() {
        let model = backfill_model_capabilities(
            ModelSpec::new("m", "openai", "gpt-4.1"),
            Some("openai"),
            None,
        );

        assert_eq!(model.context_window, Some(1_047_576));
        assert_eq!(model.max_output_tokens, Some(32_768));
        assert_eq!(model.knowledge_cutoff.as_deref(), Some("2024-06-01"));
    }

    #[test]
    fn matches_current_anthropic_family_ids() {
        let model = backfill_model_capabilities(
            ModelSpec::new("m", "anthropic", "claude-opus-4-7"),
            Some("anthropic"),
            None,
        );

        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.knowledge_cutoff.as_deref(), Some("2026-01"));
    }

    #[test]
    fn unknown_model_is_left_unmodified() {
        let model = ModelSpec::new("m", "custom", "private-model");
        assert_eq!(
            backfill_model_capabilities(model.clone(), None, None),
            model
        );
    }

    #[test]
    fn discovered_capabilities_override_static_defaults_but_not_explicit_fields() {
        let discovered = ModelCapabilityPatch {
            context_window: Some(256_000),
            max_output_tokens: Some(64_000),
            modalities: Some(Modalities {
                input: vec![Modality::Text],
                output: vec![Modality::Text],
            }),
            knowledge_cutoff: Some("2026-02".into()),
        };
        let model = ModelSpec {
            max_output_tokens: Some(4_096),
            ..ModelSpec::new("m", "openai", "gpt-4o")
        };

        let resolved = resolve_model_capabilities(model, Some("openai"), Some(&discovered));
        let filled = resolved.model;

        assert_eq!(filled.context_window, Some(256_000));
        assert_eq!(filled.max_output_tokens, Some(4_096));
        assert_eq!(filled.knowledge_cutoff.as_deref(), Some("2026-02"));
        assert_eq!(
            resolved.sources.context_window,
            Some(CapabilitySource::ProviderDiscovery)
        );
        assert_eq!(
            resolved.sources.max_output_tokens,
            Some(CapabilitySource::ExplicitSpec)
        );
        assert_eq!(
            resolved.sources.knowledge_cutoff,
            Some(CapabilitySource::ProviderDiscovery)
        );
        assert_eq!(
            filled.modalities,
            Modalities {
                input: vec![Modality::Text],
                output: vec![Modality::Text],
            }
        );
    }

    #[test]
    fn parses_openai_compatible_model_capabilities() {
        let payload = json!({
            "data": [{
                "id": "openai/gpt-4o",
                "context_length": 128000,
                "top_provider": { "max_completion_tokens": "16384" },
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                }
            }]
        });

        let parsed = parse_provider_model_capabilities("openrouter", &payload);
        let patch = parsed.get("openai/gpt-4o").expect("parsed model");

        assert_eq!(patch.context_window, Some(128_000));
        assert_eq!(patch.max_output_tokens, Some(16_384));
        assert_eq!(patch.modalities.as_ref(), Some(&vision_modalities()));
    }

    #[test]
    fn parses_gemini_model_capabilities() {
        let payload = json!({
            "models": [{
                "name": "models/gemini-2.5-pro",
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536
            }]
        });

        let parsed = parse_provider_model_capabilities("gemini", &payload);
        let patch = parsed.get("gemini-2.5-pro").expect("parsed model");

        assert_eq!(patch.context_window, Some(1_048_576));
        assert_eq!(patch.max_output_tokens, Some(65_536));
    }
}
