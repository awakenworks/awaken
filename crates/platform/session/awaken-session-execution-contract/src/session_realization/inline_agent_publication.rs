// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: Builds the one immutable executable publication a Session freezes for an Agent its
// @formal reason: Workspace does not publish; pure over its inputs, it reads no catalog, state or I/O.
// @formal reason: Classification, not a formal-proof claim.
//! The Session-scoped publication of an Agent its Workspace does not publish.
//!
//! A Worker executes only an exact Agent publication. A Session created for
//! an Agent with no Workspace publication (an inline Agent: an id plus
//! Session-level overrides) therefore freezes this publication at creation
//! and pins itself to it. The Runtime supplies the tool surface it executes;
//! everything else is the one definition of an unpublished Agent, shared with
//! the host-default snapshot a local claim pool generates per Run.

use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};

use crate::RunError;

/// The instructions of an Agent no Workspace publication authors. A Session
/// system-prompt override replaces or clears them like any publication's.
pub const UNPUBLISHED_AGENT_INSTRUCTIONS: &str =
    "You are a helpful assistant working in a local repository.";

/// The step limit of an Agent no Workspace publication authors.
pub const UNPUBLISHED_AGENT_MAX_STEPS: usize = 20;

/// The executable publication of the unpublished Agent `agent_id` on the
/// host-provisioned model `model_ref`, advertising `tools` and delegating to
/// `delegate_ids` (each at its current publication). The result is
/// content-addressed: its fingerprint is the hash of everything else.
///
/// Decision table (causes → effect):
/// | Rule | model reference | effect |
/// |---|---|---|
/// | U1 | a valid host model reference | the publication: `agent_id`'s identity, the shared instructions and step limit, a host binding of `model_ref`, exactly `tools`, `delegate_ids` as current-publication delegates; fingerprint = content hash |
/// | U2 | blank, padded or otherwise invalid | refused as a bad request naming the Agent; nothing is built |
pub fn unpublished_agent_publication(
    agent_id: &str,
    model_ref: &str,
    tools: Vec<ToolDescriptor>,
    delegate_ids: &[String],
) -> Result<ExecutableAgentSnapshot, RunError> {
    let model = ModelBinding::parse("default", model_ref, "default").map_err(|error| {
        RunError::bad_request(format!(
            "model reference `{model_ref}` of unpublished Agent `{agent_id}` is not an \
             executable binding: {error}"
        ))
    })?;
    let mut snapshot = ExecutableAgentSnapshot::builder(agent_id)
        .instructions(UNPUBLISHED_AGENT_INSTRUCTIONS)
        .model(model)
        .model_candidates(Vec::new())
        .tools(tools)
        .max_steps(UNPUBLISHED_AGENT_MAX_STEPS)
        .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
            delegates: delegate_ids
                .iter()
                .map(
                    |id| awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                        agent_id: awaken_runtime_contract::snapshot::AgentId(id.clone()),
                        source_revision: None,
                        recursive_self: false,
                    },
                )
                .collect(),
            ..Default::default()
        })
        .build();
    snapshot.recompute_fingerprint().map_err(|error| {
        RunError::internal(format!(
            "unpublished Agent `{agent_id}` publication cannot be content-addressed: {error}"
        ))
    })?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::client_executed(id, "a tool", serde_json::json!({ "type": "object" }))
    }

    #[test]
    fn unpublished_agent_publication_follows_its_decision_table() {
        // U1: identity, shared definition, exact tools/delegates, content address.
        let snapshot = unpublished_agent_publication(
            "assistant",
            "management",
            vec![tool("bash")],
            &["researcher".to_string()],
        )
        .expect("U1 valid model reference");
        assert_eq!(snapshot.root_agent_id.0, "assistant", "U1 identity");
        assert_eq!(
            snapshot.resolved_spec.instructions, UNPUBLISHED_AGENT_INSTRUCTIONS,
            "U1 instructions"
        );
        assert_eq!(
            snapshot.resolved_spec.max_steps,
            Some(UNPUBLISHED_AGENT_MAX_STEPS),
            "U1 step limit"
        );
        assert_eq!(
            snapshot.resolved_spec.model_binding.identity().1,
            "management",
            "U1 host binding of the model reference"
        );
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<Vec<_>>(),
            ["bash"],
            "U1 exactly the given tools"
        );
        assert_eq!(
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegate_ids()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            ["researcher"],
            "U1 delegates"
        );
        let mut recomputed = snapshot.clone();
        recomputed.recompute_fingerprint().unwrap();
        assert_eq!(
            recomputed.fingerprint, snapshot.fingerprint,
            "U1 content address"
        );
        assert_ne!(
            snapshot.fingerprint.0, "assistant",
            "U1 not the id fallback"
        );
        let other = unpublished_agent_publication("assistant", "other-model", Vec::new(), &[])
            .expect("U1 another model");
        assert_ne!(
            other.fingerprint, snapshot.fingerprint,
            "U1 content-addressed"
        );

        // U2: an invalid reference is refused before anything is built.
        for invalid in ["", " management", "management "] {
            let error = unpublished_agent_publication("assistant", invalid, Vec::new(), &[])
                .expect_err("U2 invalid model reference");
            assert_eq!(
                error.kind,
                awaken_runtime_contract::runs::RunErrorKind::BadRequest,
                "U2 {invalid:?}"
            );
            assert!(error.message.contains("`assistant`"), "U2 names the Agent");
        }
    }
}
