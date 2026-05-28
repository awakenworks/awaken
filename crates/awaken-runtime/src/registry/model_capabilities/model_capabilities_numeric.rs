use awaken_runtime_contract::registry_spec::ModelSpec;

use super::{CapabilitySource, ModelCapabilityPatch, ModelCapabilitySources};

pub(super) fn discovered_numeric_pair_is_usable(
    patch: &ModelCapabilityPatch,
    model_id: &str,
) -> bool {
    match (patch.context_window, patch.max_output_tokens) {
        (Some(context_window), Some(max_output_tokens))
            if context_window > 0
                && max_output_tokens > 0
                && max_output_tokens > context_window =>
        {
            tracing::warn!(
                model = model_id,
                context_window,
                max_output_tokens,
                "provider discovery ignored invalid token limit pair"
            );
            false
        }
        _ => true,
    }
}

pub(super) fn valid_context_window_candidate(
    value: u32,
    max_output_tokens: Option<u32>,
    source: CapabilitySource,
    model_id: &str,
) -> bool {
    if value == 0 {
        tracing::warn!(
            model = model_id,
            ?source,
            "ignored zero context_window capability"
        );
        return false;
    }
    if let Some(max_output_tokens) = max_output_tokens
        && max_output_tokens > 0
        && max_output_tokens > value
    {
        tracing::warn!(
            model = model_id,
            ?source,
            context_window = value,
            max_output_tokens,
            "ignored context_window capability below max_output_tokens"
        );
        return false;
    }
    true
}

pub(super) fn valid_max_output_tokens_candidate(
    value: u32,
    context_window: Option<u32>,
    source: CapabilitySource,
    model_id: &str,
) -> bool {
    if value == 0 {
        tracing::warn!(
            model = model_id,
            ?source,
            "ignored zero max_output_tokens capability"
        );
        return false;
    }
    if let Some(context_window) = context_window
        && context_window > 0
        && value > context_window
    {
        tracing::warn!(
            model = model_id,
            ?source,
            context_window,
            max_output_tokens = value,
            "ignored max_output_tokens capability above context_window"
        );
        return false;
    }
    true
}

pub(super) fn enforce_resolved_numeric_invariants(
    model: &mut ModelSpec,
    sources: &mut ModelCapabilitySources,
) {
    if model.context_window == Some(0) {
        tracing::warn!("resolved model capability dropped zero context_window");
        model.context_window = None;
        sources.context_window = None;
    }
    if model.max_output_tokens == Some(0) {
        tracing::warn!("resolved model capability dropped zero max_output_tokens");
        model.max_output_tokens = None;
        sources.max_output_tokens = None;
    }
    let (Some(context_window), Some(max_output_tokens)) =
        (model.context_window, model.max_output_tokens)
    else {
        return;
    };
    if max_output_tokens <= context_window {
        return;
    }

    if sources.max_output_tokens != Some(CapabilitySource::ExplicitSpec) {
        tracing::warn!(
            context_window,
            max_output_tokens,
            "resolved model capability dropped max_output_tokens above context_window"
        );
        model.max_output_tokens = None;
        sources.max_output_tokens = None;
    } else if sources.context_window != Some(CapabilitySource::ExplicitSpec) {
        tracing::warn!(
            context_window,
            max_output_tokens,
            "resolved model capability dropped context_window below max_output_tokens"
        );
        model.context_window = None;
        sources.context_window = None;
    } else {
        tracing::warn!(
            context_window,
            max_output_tokens,
            "resolved model capability dropped invalid explicit max_output_tokens"
        );
        model.max_output_tokens = None;
        sources.max_output_tokens = None;
    }
}

pub(super) fn sanitize_provider_capability_patch(
    model_id: &str,
    mut patch: ModelCapabilityPatch,
) -> ModelCapabilityPatch {
    if let (Some(context_window), Some(max_output_tokens)) =
        (patch.context_window, patch.max_output_tokens)
        && max_output_tokens > context_window
    {
        tracing::warn!(
            model = model_id,
            context_window,
            max_output_tokens,
            "provider discovery ignored invalid token limit pair"
        );
        patch.context_window = None;
        patch.max_output_tokens = None;
    }
    patch
}

pub(super) fn first_nonzero_u32(item: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    for key in keys {
        let Some(value) = item.get(*key).and_then(json_u32) else {
            continue;
        };
        if value == 0 {
            tracing::warn!(field = *key, "provider discovery ignored zero token limit");
            continue;
        }
        return Some(value);
    }
    None
}

fn json_u32(value: &serde_json::Value) -> Option<u32> {
    if let Some(number) = value.as_u64() {
        return u32::try_from(number).ok();
    }
    value.as_str().and_then(|string| string.parse::<u32>().ok())
}
