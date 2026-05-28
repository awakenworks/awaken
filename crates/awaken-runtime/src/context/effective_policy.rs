//! Clamp [`ContextWindowPolicy`] against [`ModelSpec`] capabilities.
//!
//! At resolve time the agent-declared policy must be narrowed to the
//! model's published `context_window` / `max_output_tokens` so the
//! runtime never issues a request the provider rejects.

use awaken_runtime_contract::contract::inference::ContextWindowPolicy;
use awaken_runtime_contract::registry_spec::ModelSpec;

/// Return a [`ContextWindowPolicy`] clamped against the model's published
/// capabilities.
///
/// When a `ModelSpec` declares `context_window` and/or `max_output_tokens`,
/// the agent's configured policy must never exceed those values — otherwise
/// the runtime would issue requests the provider rejects. A capability set
/// to `None` means "model did not declare a limit" and is passed through
/// unchanged. The function is monotonically narrowing: it never widens any
/// field, never mutates the input, and preserves every non-clamped field
/// (recent-message floor, prompt caching, autocompact threshold, ...).
pub fn effective_policy(policy: &ContextWindowPolicy, model: &ModelSpec) -> ContextWindowPolicy {
    let mut out = policy.clone();
    if let Some(cap) = model.context_window {
        out.max_context_tokens = out.max_context_tokens.min(cap as usize);
    }
    if let Some(cap) = model.max_output_tokens {
        out.max_output_tokens = out.max_output_tokens.min(cap as usize);
    }
    // Invariant: max_output_tokens never exceeds max_context_tokens. A model
    // that publishes only `context_window` (no output cap) must still leave
    // the output budget within the clamped context window; likewise a stale
    // user policy with inverted values gets corrected here.
    out.max_output_tokens = out.max_output_tokens.min(out.max_context_tokens);

    // autocompact_threshold must fire while there is still room to compact,
    // i.e. before raw tokens consume the entire usable input budget
    // (max_context_tokens - max_output_tokens). If usable_input is 0 (output
    // reservation equals or exceeds context), drop the threshold since
    // compaction cannot meaningfully run.
    if let Some(threshold) = out.autocompact_threshold {
        let usable_input = out.max_context_tokens.saturating_sub(out.max_output_tokens);
        out.autocompact_threshold = if usable_input == 0 {
            None
        } else {
            Some(threshold.min(usable_input))
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_policy_clamps_max_context_to_model_capability() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(32_000),
            max_output_tokens: Some(4_096),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 32_000);
        assert_eq!(eff.max_output_tokens, 4_096);
    }

    #[test]
    fn effective_policy_passes_through_when_model_has_no_caps() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            ..Default::default()
        };
        let model = ModelSpec::new("m", "p", "u");
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 200_000);
        assert_eq!(eff.max_output_tokens, 16_384);
    }

    #[test]
    fn effective_policy_only_narrows_never_widens() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 8_000,
            max_output_tokens: 2_000,
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(1_000_000),
            max_output_tokens: Some(100_000),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 8_000);
        assert_eq!(eff.max_output_tokens, 2_000);
    }

    #[test]
    fn effective_policy_preserves_other_fields() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            min_recent_messages: 7,
            enable_prompt_cache: false,
            // Stays well below usable_input (100_000 - 16_384 = 83_616) so the
            // invariant clamp is a no-op and the value passes through.
            autocompact_threshold: Some(50_000),
            compaction_raw_suffix_messages: 5,
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(100_000),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 100_000);
        assert_eq!(eff.min_recent_messages, 7);
        assert!(!eff.enable_prompt_cache);
        assert_eq!(eff.autocompact_threshold, Some(50_000));
        assert_eq!(eff.compaction_raw_suffix_messages, 5);
    }

    #[test]
    fn effective_policy_caps_output_at_clamped_context_when_model_lacks_output_cap() {
        // Reviewer's Breach A scenario.
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(8_000),
            max_output_tokens: None,
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 8_000);
        assert!(
            eff.max_output_tokens <= eff.max_context_tokens,
            "output {} must not exceed context {}",
            eff.max_output_tokens,
            eff.max_context_tokens
        );
        assert_eq!(eff.max_output_tokens, 8_000);
    }

    #[test]
    fn effective_policy_caps_output_when_policy_alone_inverts_invariant() {
        // policy.max_output_tokens > policy.max_context_tokens; even with no
        // model caps, effective_policy should restore the invariant.
        let policy = ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 8_000, // user error / stale config
            ..Default::default()
        };
        let model = ModelSpec::new("m", "p", "u");
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 4_000);
        assert_eq!(eff.max_output_tokens, 4_000);
    }

    #[test]
    fn effective_policy_clamps_autocompact_threshold_to_usable_input() {
        // Reviewer's Breach B scenario, sharpened: threshold must fit within
        // (context - output) so compaction can actually fire.
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            autocompact_threshold: Some(150_000),
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(100_000),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        let usable = eff.max_context_tokens - eff.max_output_tokens; // 100_000 - 16_384
        assert_eq!(eff.autocompact_threshold, Some(usable));
    }

    #[test]
    fn effective_policy_drops_autocompact_threshold_when_no_usable_input() {
        // Pathological: model.max_output_tokens == context_window leaves no input room.
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            autocompact_threshold: Some(50_000),
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(8_000),
            max_output_tokens: Some(8_000),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.max_context_tokens, 8_000);
        assert_eq!(eff.max_output_tokens, 8_000);
        assert_eq!(
            eff.autocompact_threshold, None,
            "no usable input budget => no point in auto-compaction"
        );
    }

    #[test]
    fn effective_policy_leaves_autocompact_threshold_below_usable_input_untouched() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            autocompact_threshold: Some(50_000), // well under usable_input
            ..Default::default()
        };
        let model = ModelSpec {
            context_window: Some(200_000),
            max_output_tokens: Some(16_384),
            ..ModelSpec::new("m", "p", "u")
        };
        let eff = effective_policy(&policy, &model);
        assert_eq!(eff.autocompact_threshold, Some(50_000));
    }
}
