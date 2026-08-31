//! Placement and descriptor-kind drift guards for the closed builtin catalog.

use awaken_ext_builtin_tools::{Toolset, all_hand_tools, builtin_tools};
use awaken_runtime_contract::tool::ToolExecutionTarget;
use std::collections::BTreeSet;

#[test]
fn hand_tool_execution_targets_follow_the_placement_decision_table() {
    // Cause/effect graph: C1 the six filesystem/shell implementations enter
    // through the canonical `all_hand_tools` registry. E1 every member targets
    // Sandbox and is therefore dispatched only by the SessionEnvironment-owned
    // executor.
    // Constraint K1: `all_hand_tools` is the closed canonical union; configured
    // `web_fetch` and `web_search` retain their separate plugin owners and are
    // not second members. Decision rule P1=C1=>E1. Coverage rationale: iterating
    // the closed registry covers all six static placements, while the canonical
    // unit test in `lib.rs` guards the exact descriptor/executor membership.
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
    // Cause/effect graph: C1 Hand and C2 Coordination are ordinary executable
    // families; C3 Delegation enters the kernel-owned resolver path.
    // Effects: E1 C1/C2 => Regular; E2 C3 => AgentDelegation.
    // Constraints: `BuiltinTool` has no public constructor, mutable fields, or
    // serde input, so the decision table enumerates every constructible pairing.
    // Decision rules: R1=C1|C2=>E1; R2=C3=>E2.
    for tool in builtin_tools() {
        let expected = match tool.toolset() {
            Toolset::Hand | Toolset::Coordination => {
                awaken_runtime_contract::resolved::ToolKind::Regular
            }
            Toolset::Delegation => awaken_runtime_contract::resolved::ToolKind::AgentDelegation,
        };
        assert_eq!(tool.descriptor().kind, expected);
    }
}

#[test]
fn builtin_catalog_is_the_exact_non_overlapping_command_surface() {
    // Cause/effect graph: C1 native delegation uses `agent_run`; C2 Managed
    // coordination uses `list_agents`/`send_to_agent`; C3 the abandoned Task
    // model commands name overlapping ingress/control/recovery effects.
    // Effects: E1 C1/C2 remain in the one closed catalog; E2 C3 and every
    // Git/repository-specific command are absent because Bash is their sole owner.
    //
    // | Rule | command family | catalog effect |
    // | R1 | native delegation | exactly `agent_run` |
    // | R2 | Managed coordination | exactly list/send |
    // | R3 | legacy Task, Git, repository, move/delete commands | none |
    //
    // This is a membership guard, not a string-only implementation test: the
    // preceding exhaustive Toolset match also proves there is no Task family
    // into which these ids could be registered.
    let ids = builtin_tools()
        .into_iter()
        .map(|tool| tool.into_descriptor().id)
        .collect::<BTreeSet<_>>();
    let expected = [
        "agent_run",
        "bash",
        "edit",
        "glob",
        "grep",
        "list_agents",
        "read",
        "send_to_agent",
        "write",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    assert_eq!(ids, expected, "R1-R3/E1-E2");
}
