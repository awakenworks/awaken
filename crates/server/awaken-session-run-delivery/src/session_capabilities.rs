//! The capabilities a Session advertises: the registered built-in Hand tools
//! (each with whether it asks before executing), and the Managed Agents
//! mid-conversation system-message support of the model a Session selects.

use std::collections::HashSet;

use awaken_ext_builtin_tools::{Toolset, all_hand_tools, builtin_tools};
use awaken_runtime_contract::resolved::ToolDescriptor;

/// The descriptors of the registered built-in Hand tools.
pub fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
    let registered: HashSet<String> = all_hand_tools()
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    builtin_tools()
        .into_iter()
        .filter(|tool| {
            tool.toolset() == Toolset::Hand && registered.contains(&tool.descriptor().id)
        })
        .map(|tool| tool.into_descriptor())
        .collect()
}

/// The registered built-in Hand tools advertised on a managed Session. The ask
/// bit projects the same controlled-modification predicate as authorization;
/// exact typed overrides are applied separately from the Session configuration.
pub fn builtin_hand_tools() -> Vec<(String, bool)> {
    hand_tool_descriptors()
        .into_iter()
        .map(|descriptor| {
            let ask =
                !awaken_session_configuration_contract::is_agent_toolset_member(&descriptor.id)
                    || awaken_session_configuration_contract::is_controlled_modification_member(
                        &descriptor.id,
                    );
            (descriptor.id, ask)
        })
        .collect()
}

/// Claude model capability named by the Managed Agents contract. Awaken-native
/// non-Claude executors own their system-message support through the neutral
/// runtime and remain supported; Claude ids fail closed unless they are one of
/// the documented mid-conversation families. Dated aliases share the family
/// prefix (for example `claude-opus-5-20260701`).
pub fn supports_mid_conversation_system(model: &str) -> bool {
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
