//! Drift guards tying the `Toolset::Hand` descriptors to the erased hand-tool
//! implementations and the closed built-in catalog pairings.

use awaken_ext_builtin_tools::{Toolset, all_hand_tools, builtin_tools, executable_hand_tools};
use awaken_runtime_contract::tool::ToolExecutionTarget;
use std::collections::BTreeSet;

/// The static `hand` toolset's model-visible descriptors and registered
/// implementations must match exactly. The two Web capabilities belong only to
/// their configured plugins, so neither may enter this catalog.
#[test]
fn hand_descriptors_exactly_cover_the_erased_hand_tool_implementations() {
    // Registry cause/effect rules: C1 every static hand descriptor -> E1 one
    // static implementation; C2 every static implementation -> E2 one
    // descriptor; C3 WebFetch/WebSearch -> E3 absent from both sets because
    // their configured plugins are the single execution owners. Decision rules:
    // R1=C1|C2 => exact set equality; R2=C3 => neither Web id is present.
    let descriptor_ids: BTreeSet<String> = builtin_tools()
        .into_iter()
        .filter(|tool| tool.toolset() == Toolset::Hand)
        .map(|tool| tool.into_descriptor().id)
        .collect();

    let implementation_ids: BTreeSet<String> = all_hand_tools()
        .iter()
        .map(|tool| tool.id().to_string())
        .collect();

    assert_eq!(
        descriptor_ids.len(),
        8,
        "the static hand toolset is exactly the 8 sandbox descriptors"
    );
    assert_eq!(
        descriptor_ids, implementation_ids,
        "every hand descriptor has a matching erased implementation and vice versa"
    );
    assert!(!descriptor_ids.contains("web_fetch"), "R2/E3");
    assert!(!descriptor_ids.contains("web_search"), "R2/E3");
    assert_eq!(executable_hand_tools().len(), 8);
}

#[test]
fn hand_tool_execution_targets_follow_the_placement_decision_table() {
    // Cause/effect graph: C1 the eight filesystem/shell implementations enter
    // through `executable_hand_tools`. E1 every member targets Sandbox and is
    // therefore dispatched only by the SessionEnvironment-owned executor.
    // Constraint K1: `all_hand_tools` is the closed canonical union; configured
    // `web_fetch` and `web_search` retain their separate plugin owners and are
    // not second members. Decision rule P1=C1=>E1. Coverage rationale: iterating
    // the closed registry covers all eight static placements, while the preceding
    // membership test guards the exact dependency-expanded domain.
    for tool in all_hand_tools() {
        assert_eq!(
            tool.execution_target(),
            ToolExecutionTarget::Sandbox,
            "{} must execute in the sandbox",
            tool.id()
        );
    }
}

#[test]
fn grep_descriptor_exposes_every_executor_input() {
    // Cause/effect graph: C1 GrepTool requires a regex pattern; C2 it accepts an
    // optional file/directory path and defaults to the current directory. This
    // is the official Managed Agent tool shape while still allowing spill-file
    // recovery by passing `path` explicitly.
    let grep = builtin_tools()
        .into_iter()
        .find(|tool| tool.descriptor().id == "grep")
        .expect("grep descriptor");
    let properties = grep.descriptor().parameters["properties"]
        .as_object()
        .expect("G1 properties");
    assert!(properties.contains_key("pattern"), "G1/C1");
    assert!(properties.contains_key("path"), "G1/C2");
    assert_eq!(
        grep.descriptor().parameters["required"],
        serde_json::json!(["pattern"]),
        "G1/E1"
    );
}

#[test]
fn catalog_pairs_each_execution_family_with_its_only_legal_descriptor_kind() {
    // Cause/effect graph: C1 Hand, C2 Task, and C3 Coordination are ordinary
    // executable families; C4 Delegation enters the kernel-owned resolver path.
    // Effects: E1 C1/C2/C3 => Regular; E2 C4 => AgentDelegation.
    // Constraints: `BuiltinTool` has no public constructor, mutable fields, or
    // serde input, so the decision table enumerates every constructible pairing.
    // Decision rules: R1=C1|C2|C3=>E1; R2=C4=>E2.
    for tool in builtin_tools() {
        let expected = match tool.toolset() {
            Toolset::Hand | Toolset::Task | Toolset::Coordination => {
                awaken_runtime_contract::resolved::ToolKind::Regular
            }
            Toolset::Delegation => awaken_runtime_contract::resolved::ToolKind::AgentDelegation,
        };
        assert_eq!(tool.descriptor().kind, expected);
    }
}
