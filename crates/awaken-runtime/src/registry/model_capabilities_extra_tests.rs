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

#[test]
fn resolved_capabilities_skip_discovered_max_above_explicit_context() {
    let discovered = ModelCapabilityPatch {
        context_window: None,
        max_output_tokens: Some(65_536),
        modalities: None,
        knowledge_cutoff: None,
    };
    let model = ModelSpec {
        context_window: Some(4_096),
        ..ModelSpec::new("m", "openai", "gpt-4o")
    };

    let resolved = resolve_model_capabilities(model, Some("openai"), Some(&discovered));

    assert_eq!(resolved.model.context_window, Some(4_096));
    assert_eq!(resolved.model.max_output_tokens, None);
    assert_eq!(
        resolved.sources.context_window,
        Some(CapabilitySource::ExplicitSpec)
    );
    assert_eq!(resolved.sources.max_output_tokens, None);
}

#[test]
fn resolved_capabilities_skip_invalid_discovered_token_pair() {
    let discovered = ModelCapabilityPatch {
        context_window: Some(4_096),
        max_output_tokens: Some(8_192),
        modalities: None,
        knowledge_cutoff: None,
    };

    let resolved = resolve_model_capabilities(
        ModelSpec::new("m", "custom", "private-model"),
        None,
        Some(&discovered),
    );

    assert_eq!(resolved.model.context_window, None);
    assert_eq!(resolved.model.max_output_tokens, None);
    assert_eq!(resolved.sources.context_window, None);
    assert_eq!(resolved.sources.max_output_tokens, None);
}

#[test]
fn provider_parser_drops_zero_token_limits() {
    let payload = json!({
        "data": [{
            "id": "zero-context",
            "context_window": 0,
            "max_output_tokens": 1024
        }, {
            "id": "zero-max",
            "context_window": 4096,
            "max_output_tokens": "0"
        }]
    });

    let parsed = parse_provider_model_capabilities("openai", &payload);

    let zero_context = parsed.get("zero-context").expect("parsed zero-context");
    assert_eq!(zero_context.context_window, None);
    assert_eq!(zero_context.max_output_tokens, Some(1_024));

    let zero_max = parsed.get("zero-max").expect("parsed zero-max");
    assert_eq!(zero_max.context_window, Some(4_096));
    assert_eq!(zero_max.max_output_tokens, None);
}

#[test]
fn provider_parser_drops_invalid_discovered_token_pair() {
    let payload = json!({
        "data": [{
            "id": "invalid-pair",
            "context_window": 4096,
            "max_output_tokens": 8192
        }]
    });

    let parsed = parse_provider_model_capabilities("openai", &payload);

    assert!(
        parsed.is_empty(),
        "invalid provider token pair must not enter the trusted discovery map"
    );
}
