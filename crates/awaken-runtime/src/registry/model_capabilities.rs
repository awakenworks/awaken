//! Built-in model capability defaults used during registry resolution.
//!
//! The serialized `ModelSpec` remains authoritative. These defaults only fill
//! omitted capability fields so UI-created lightweight model entries still
//! receive conservative runtime bounds for common hosted models.

use awaken_contract::registry_spec::{Modalities, Modality, ModelSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityDefaults {
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    modalities: Option<Modalities>,
    knowledge_cutoff: Option<&'static str>,
}

impl CapabilityDefaults {
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
        knowledge_cutoff: &'static str,
    ) -> Self {
        Self {
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            modalities: Some(vision_modalities()),
            knowledge_cutoff: Some(knowledge_cutoff),
        }
    }
}

pub(crate) fn backfill_model_capabilities(
    mut model: ModelSpec,
    provider_source: Option<&str>,
) -> ModelSpec {
    let defaults = provider_source
        .and_then(|source| lookup(source, &model.upstream_model))
        .or_else(|| lookup(&model.provider_id, &model.upstream_model));

    let Some(defaults) = defaults else {
        return model;
    };

    if model.context_window.is_none() {
        model.context_window = defaults.context_window;
    }
    if model.max_output_tokens.is_none() {
        model.max_output_tokens = defaults.max_output_tokens;
    }
    if model.modalities.input.is_empty()
        && model.modalities.output.is_empty()
        && let Some(modalities) = defaults.modalities
    {
        model.modalities = modalities;
    }
    if model.knowledge_cutoff.is_none() {
        model.knowledge_cutoff = defaults.knowledge_cutoff.map(str::to_owned);
    }

    model
}

fn lookup(provider: &str, upstream_model: &str) -> Option<CapabilityDefaults> {
    let provider = provider.to_ascii_lowercase();
    let model = normalize_model_name(upstream_model);
    match provider.as_str() {
        "openai" => openai_defaults(&model),
        "anthropic" => anthropic_defaults(&model),
        "gemini" | "google" | "vertex" => gemini_defaults(&model),
        "openrouter" => openrouter_defaults(&model),
        _ => None,
    }
}

fn openrouter_defaults(model: &str) -> Option<CapabilityDefaults> {
    let (_, routed) = model.split_once('/')?;
    openai_defaults(routed)
        .or_else(|| anthropic_defaults(routed))
        .or_else(|| gemini_defaults(routed))
}

fn openai_defaults(model: &str) -> Option<CapabilityDefaults> {
    if model == "gpt-5.5" || model == "gpt-5.5-pro" {
        return Some(CapabilityDefaults::vision_with_cutoff(
            1_050_000,
            128_000,
            "2025-12-01",
        ));
    }
    if model == "gpt-5.4" {
        return Some(CapabilityDefaults::vision_with_cutoff(
            1_050_000,
            128_000,
            "2025-08-31",
        ));
    }
    if model == "gpt-4o" || model.starts_with("gpt-4o-") {
        return Some(CapabilityDefaults::vision(128_000, 16_384));
    }
    if model == "gpt-4o-mini" || model.starts_with("gpt-4o-mini-") {
        return Some(CapabilityDefaults::vision(128_000, 16_384));
    }
    if model == "gpt-4.1"
        || model.starts_with("gpt-4.1-")
        || model == "gpt-4.1-mini"
        || model.starts_with("gpt-4.1-mini-")
        || model == "gpt-4.1-nano"
        || model.starts_with("gpt-4.1-nano-")
    {
        return Some(CapabilityDefaults::vision_with_cutoff(
            1_047_576,
            32_768,
            "2024-06-01",
        ));
    }
    if model == "o3" || model.starts_with("o3-") || model == "o4-mini" {
        return Some(CapabilityDefaults::vision(200_000, 100_000));
    }
    if model == "o1" || model.starts_with("o1-") {
        return Some(CapabilityDefaults::vision(200_000, 100_000));
    }
    None
}

fn anthropic_defaults(model: &str) -> Option<CapabilityDefaults> {
    if model == "claude-opus-4-7" {
        return Some(CapabilityDefaults::vision_with_cutoff(
            1_000_000, 128_000, "2026-01",
        ));
    }
    if model == "claude-sonnet-4-6" {
        return Some(CapabilityDefaults::vision_with_cutoff(
            1_000_000, 64_000, "2025-08",
        ));
    }
    if model.starts_with("claude-haiku-4-5") {
        return Some(CapabilityDefaults::vision_with_cutoff(
            200_000, 64_000, "2025-02",
        ));
    }
    if model.starts_with("claude-opus-4-") || model.starts_with("claude-sonnet-4-") {
        return Some(CapabilityDefaults::vision(200_000, 32_000));
    }
    if model.starts_with("claude-3-")
        || model.starts_with("claude-opus-3-")
        || model.starts_with("claude-sonnet-3-")
        || model.starts_with("claude-haiku-3-")
    {
        return Some(CapabilityDefaults::vision(200_000, 8_192));
    }
    None
}

fn gemini_defaults(model: &str) -> Option<CapabilityDefaults> {
    if model.starts_with("gemini-1.5-") || model.starts_with("gemini-2.0-") {
        return Some(CapabilityDefaults::multimodal(1_048_576, 8_192));
    }
    if model.starts_with("gemini-2.5-") {
        return Some(CapabilityDefaults::multimodal(1_048_576, 65_536));
    }
    None
}

fn normalize_model_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_missing_openai_capabilities() {
        let model =
            backfill_model_capabilities(ModelSpec::new("m", "openai", "gpt-4o"), Some("openai"));

        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.max_output_tokens, Some(16_384));
        assert_eq!(model.modalities, vision_modalities());
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
            backfill_model_capabilities(explicit.clone(), Some("openai")),
            explicit
        );
    }

    #[test]
    fn provider_source_handles_provider_aliases() {
        let model = backfill_model_capabilities(
            ModelSpec::new("m", "prod-openai", "gpt-4o-mini"),
            Some("openai"),
        );

        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.max_output_tokens, Some(16_384));
    }

    #[test]
    fn fills_known_knowledge_cutoff_when_available() {
        let model =
            backfill_model_capabilities(ModelSpec::new("m", "openai", "gpt-4.1"), Some("openai"));

        assert_eq!(model.context_window, Some(1_047_576));
        assert_eq!(model.max_output_tokens, Some(32_768));
        assert_eq!(model.knowledge_cutoff.as_deref(), Some("2024-06-01"));
    }

    #[test]
    fn matches_current_anthropic_family_ids() {
        let model = backfill_model_capabilities(
            ModelSpec::new("m", "anthropic", "claude-opus-4-7"),
            Some("anthropic"),
        );

        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.knowledge_cutoff.as_deref(), Some("2026-01"));
    }

    #[test]
    fn unknown_model_is_left_unmodified() {
        let model = ModelSpec::new("m", "custom", "private-model");
        assert_eq!(backfill_model_capabilities(model.clone(), None), model);
    }
}
