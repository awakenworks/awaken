//! Drift guards tying the `Toolset::Hand` descriptors to the erased hand-tool
//! implementations, plus the `BuiltinTool` / `Toolset` serde wire shape.

use awaken_ext_builtin_tools::{
    BuiltinTool, Toolset, all_hand_tools, builtin_tools, executable_hand_tools, web_hand_tools,
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
        .filter(|tool| tool.toolset == Toolset::Hand)
        .map(|tool| tool.descriptor.id)
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
        .find(|tool| tool.descriptor.id == "grep")
        .expect("grep descriptor");
    let properties = grep.descriptor.parameters["properties"]
        .as_object()
        .expect("G1 properties");
    assert!(properties.contains_key("pattern"), "G1/C1");
    assert!(properties.contains_key("path"), "G1/C2");
    assert_eq!(
        grep.descriptor.parameters["required"],
        serde_json::json!(["pattern"]),
        "G1/E1"
    );
}

#[test]
fn builtin_tool_round_trips_through_json() {
    // Any concrete descriptor is enough to exercise the whole `BuiltinTool` wire
    // shape (toolset tag + nested `ToolDescriptor`).
    let original = builtin_tools()
        .into_iter()
        .next()
        .expect("at least one builtin");
    let json = serde_json::to_value(&original).expect("serialize");
    let back: BuiltinTool = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, original);
}

#[test]
fn toolset_variants_round_trip() {
    for variant in [Toolset::Hand, Toolset::Task, Toolset::Delegation] {
        let json = serde_json::to_value(variant).expect("serialize");
        let back: Toolset = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, variant);
    }
    // The wire tokens are the variant names (no rename attribute).
    assert_eq!(serde_json::to_value(Toolset::Hand).unwrap(), "Hand");
    assert_eq!(serde_json::to_value(Toolset::Task).unwrap(), "Task");
    assert_eq!(
        serde_json::to_value(Toolset::Delegation).unwrap(),
        "Delegation"
    );
}
