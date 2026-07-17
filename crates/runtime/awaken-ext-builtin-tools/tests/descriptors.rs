//! Drift guards tying the `Toolset::Hand` descriptors to the erased hand-tool
//! implementations, plus the `BuiltinTool` / `Toolset` serde wire shape.

use awaken_ext_builtin_tools::{
    BuiltinTool, Toolset, builtin_tools, executable_hand_tools, web_hand_tools,
};
use std::collections::BTreeSet;

/// The `hand` toolset's model-visible descriptors and the registered
/// implementations (`executable_hand_tools` + `web_hand_tools`) must name exactly
/// the same 8 tool ids — a descriptor with no implementation (or vice versa) would
/// be a model-callable tool that never runs, or an unreachable implementation.
#[test]
fn hand_descriptors_exactly_cover_the_erased_hand_tool_implementations() {
    let descriptor_ids: BTreeSet<String> = builtin_tools()
        .into_iter()
        .filter(|tool| tool.toolset == Toolset::Hand)
        .map(|tool| tool.descriptor.id)
        .collect();

    let implementation_ids: BTreeSet<String> = executable_hand_tools()
        .iter()
        .chain(web_hand_tools().iter())
        .map(|tool| tool.id().to_string())
        .collect();

    assert_eq!(
        descriptor_ids.len(),
        8,
        "the hand toolset is exactly the 8 in-process descriptors"
    );
    assert_eq!(
        descriptor_ids, implementation_ids,
        "every hand descriptor has a matching erased implementation and vice versa"
    );
    // The split between the two constructors is 6 local + 2 network = 8.
    assert_eq!(executable_hand_tools().len(), 6);
    assert_eq!(web_hand_tools().len(), 2);
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
