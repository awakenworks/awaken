use super::*;
use serde_json::json;

#[test]
fn output_only_explicit_modalities_keep_distinct_sources() {
    let model = ModelSpec {
        modalities: Modalities {
            input: Vec::new(),
            output: vec![Modality::Text],
        },
        ..ModelSpec::new("m", "openai", "gpt-4o")
    };

    let resolved = resolve_model_capabilities(model, Some("openai"), None);

    assert_eq!(
        resolved.sources.input_modalities,
        Some(CapabilitySource::StaticHeuristic)
    );
    assert_eq!(
        resolved.sources.output_modalities,
        Some(CapabilitySource::ExplicitSpec)
    );
}

#[test]
fn provider_modalities_do_not_require_text_input() {
    let payload = json!({
        "data": [{
            "id": "vision-only",
            "architecture": {
                "input_modalities": ["image"],
                "output_modalities": ["text"]
            }
        }]
    });

    let parsed = parse_provider_model_capabilities("openai", &payload);
    let patch = parsed.get("vision-only").expect("parsed model");

    assert_eq!(
        patch.modalities.as_ref(),
        Some(&Modalities {
            input: vec![Modality::Image],
            output: vec![Modality::Text],
        })
    );
}

#[test]
fn gemini_token_discovery_keeps_static_media_modalities() {
    let discovered = ModelCapabilityPatch {
        context_window: Some(1_048_576),
        max_output_tokens: Some(65_536),
        modalities: None,
        knowledge_cutoff: None,
    };

    let resolved = resolve_model_capabilities(
        ModelSpec::new("m", "gemini", "gemini-2.5-pro"),
        Some("gemini"),
        Some(&discovered),
    );

    assert_eq!(resolved.model.context_window, Some(1_048_576));
    assert_eq!(resolved.model.max_output_tokens, Some(65_536));
    assert_eq!(resolved.model.modalities, multimodal_modalities());
    assert_eq!(
        resolved.sources.context_window,
        Some(CapabilitySource::ProviderDiscovery)
    );
    assert_eq!(
        resolved.sources.max_output_tokens,
        Some(CapabilitySource::ProviderDiscovery)
    );
    assert_eq!(
        resolved.sources.input_modalities,
        Some(CapabilitySource::StaticHeuristic)
    );
    assert_eq!(
        resolved.sources.output_modalities,
        Some(CapabilitySource::StaticHeuristic)
    );
}
