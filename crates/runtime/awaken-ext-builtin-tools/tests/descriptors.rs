//! Drift guards tying the `Toolset::Hand` descriptors to the erased hand-tool
//! implementations and the closed built-in catalog pairings.

use awaken_ext_builtin_tools::{
    Toolset, all_hand_tools, builtin_tools, executable_hand_tools, web_hand_tools,
};
use awaken_runtime_contract::tool::ToolExecutionTarget;
use std::collections::BTreeSet;

/// The `hand` toolset's model-visible descriptors and the registered
/// implementations (`executable_hand_tools` + `web_hand_tools`) must name exactly
/// the same 9 tool ids — a descriptor with no implementation (or vice versa) would
/// be a model-callable tool that never runs, or an unreachable implementation.
#[test]
fn hand_descriptors_exactly_cover_the_erased_hand_tool_implementations() {
    // Registry cause/effect rules: C1 every visible hand descriptor -> E1 one
    // executable implementation; C2 every implementation -> E2 one descriptor;
    // C3 the two single-file lifecycle tools are installed -> E3 the canonical
    // registry contains 8 local tools plus the one web fetch implementation.
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
        9,
        "the hand toolset is exactly the 9 in-process descriptors"
    );
    assert_eq!(
        descriptor_ids, implementation_ids,
        "every hand descriptor has a matching erased implementation and vice versa"
    );
    // Search has one configurable plugin owner; the static split is 8 local + fetch.
    assert_eq!(executable_hand_tools().len(), 8);
    assert_eq!(web_hand_tools().len(), 1);
}

#[test]
fn hand_tool_execution_targets_follow_the_placement_decision_table() {
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
    // Decision table: C1 Hand => Regular; C2 Task => Regular;
    // C3 Delegation => AgentDelegation. `BuiltinTool` has no public constructor,
    // mutable fields, or serde input, so these are the complete constructible
    // states rather than validation of an open tuple.
    for tool in builtin_tools() {
        let expected = match tool.toolset() {
            Toolset::Hand | Toolset::Task => awaken_runtime_contract::resolved::ToolKind::Regular,
            Toolset::Delegation => awaken_runtime_contract::resolved::ToolKind::AgentDelegation,
        };
        assert_eq!(tool.descriptor().kind, expected);
    }
}
