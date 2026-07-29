//! Managed-only model capability decisions at the SessionRuntime adapter edge.
//! The shared Host exposes the effective per-thread model; this module maps that
//! neutral fact to the Managed Agents mid-conversation system-message contract.

/// Claude model capability named by the Managed Agents contract. Awaken-native
/// non-Claude executors own their system-message support through the neutral
/// runtime and remain supported; Claude ids fail closed unless they are one of
/// the documented mid-conversation families. Dated aliases share the family
/// prefix (for example `claude-opus-5-20260701`).
pub(crate) fn supports_mid_conversation_system(model: &str) -> bool {
    let model = model.to_ascii_lowercase().replace('.', "-");
    if !model.starts_with("claude-") {
        return true;
    }
    [
        "claude-opus-4-8",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
    ]
    .into_iter()
    .any(|family| model == family || model.starts_with(&format!("{family}-")))
}

#[cfg(test)]
mod tests {
    use super::supports_mid_conversation_system;

    /// Cause/effect table: C1 Claude family vs non-Claude; C2 documented family
    /// vs other Claude family; C3 canonical/dotted/dated alias. Effects are admit
    /// or reject before persistence. H1/H2 admit the four supported families and
    /// native non-Claude models; H4 rejects every other Claude family.
    #[test]
    fn mid_conversation_system_capability_is_model_specific_and_fail_closed_for_claude() {
        for model in [
            "claude-opus-4.8",
            "claude-fable-5-20260701",
            "claude-mythos-5",
            "claude-opus-5",
            "kimi-k2",
        ] {
            assert!(supports_mid_conversation_system(model), "H1/H2 {model}");
        }
        for model in ["claude-sonnet-4-5", "claude-haiku-4-5", "claude-opus-4-7"] {
            assert!(!supports_mid_conversation_system(model), "H4 {model}");
        }
    }
}
