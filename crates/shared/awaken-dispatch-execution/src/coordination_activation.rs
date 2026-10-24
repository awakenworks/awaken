//! Execution lowers Session coordination commands into the SDK activation.
//! The same immutable input rules bind an original accepted activation without
//! selecting current model, resource or inherited tool configuration.

use awaken_agent_contract::agent::message::Role;
use awaken_session_execution_contract::{
    CoordinatedAdmissionRef, CoordinatedRunCommand, CoordinatedRunIntent,
    coordinated_activation_input, coordinated_run_id, coordinated_thread_id,
};

fn coordinated_origin_id(
    session: &str,
    operation: &str,
) -> awaken_agent_contract::agent::delegation::DelegationId {
    awaken_agent_contract::agent::delegation::DelegationId(format!(
        "coordination:{}",
        awaken_agent_contract::stable_fingerprint(&(session, operation))
    ))
}

/// Freeze the same existing activation after its Session-owned projection.
pub fn coordinated_run_activation(
    command: CoordinatedRunCommand,
    ancestor_agent: String,
    caller_toolsets: Vec<awaken_agent_contract::toolset::ToolsetPolicy>,
) -> awaken_runtime_contract::activation::RunActivation {
    let input = coordinated_activation_input(
        command.session_id.as_str(),
        &command.thread_id,
        &command.run_id,
        &command.operation_id,
        command.message,
    );
    let origin = awaken_agent_contract::agent::delegation::DelegationOrigin {
        delegation_id: coordinated_origin_id(command.session_id.as_str(), &command.operation_id),
        parent_run_id: command.parent_run_id,
        parent_call_id: command.parent_call_id,
        depth: 1,
        agent_lineage: vec![ancestor_agent],
    };
    awaken_runtime_contract::activation::RunActivation::new(
        command.run_id,
        command.thread_id,
        command.snapshot,
        input,
    )
    .with_delegation_origin(origin)
    .with_inherited_toolsets(Some(
        awaken_agent_contract::toolset::InheritedToolsets::with_caller(caller_toolsets, None),
    ))
}

/// Exact original business binding, not another dispatch hash/identity law.
pub fn coordinated_activation_matches(
    command: &CoordinatedAdmissionRef<'_>,
    a: &awaken_runtime_contract::activation::RunActivation,
) -> bool {
    if a.run_id != command.run_id() || a.thread_id != command.thread_id() {
        return false;
    }
    match command {
        CoordinatedAdmissionRef::Child(c) => {
            c.thread_id.0 != c.session_id.as_str()
                && c.run_id == coordinated_run_id(&c.session_id, &c.thread_id, &c.parent_run_id, &c.operation_id)
                && (c.intent != CoordinatedRunIntent::Spawn || c.thread_id == coordinated_thread_id(&c.session_id, &c.parent_run_id, &c.operation_id))
                && a.snapshot.root_agent_id == c.snapshot.root_agent_id
                && a.snapshot.metadata.source == c.snapshot.metadata.source
                && c.snapshot.metadata.source.revision > 0
                && c.snapshot.metadata.source.agent_id == c.snapshot.root_agent_id
                && a.delegation_origin.as_ref().is_some_and(|o| {
                    o.delegation_id == coordinated_origin_id(c.session_id.as_str(), &c.operation_id)
                        && o.parent_run_id == c.parent_run_id && o.parent_call_id == c.parent_call_id
                        && o.depth == 1 && o.agent_lineage.len() == 1 && !o.agent_lineage[0].is_empty()
                })
                && a.inherited_toolsets().is_some()
                && a.tool_capability_narrowing() == awaken_runtime_contract::permission::ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets
                && a.input == coordinated_activation_input(c.session_id.as_str(), &c.thread_id, &c.run_id, &c.operation_id, c.message.clone())
        }
        CoordinatedAdmissionRef::Report(c, baseline) => a.snapshot.root_agent_id.0 == baseline.agent_id
            && a.snapshot.metadata.source.agent_id.0 == baseline.agent_id
            && baseline.agent_revision.is_some_and(|revision| revision > 0 && a.snapshot.metadata.source.revision == revision)
            && c.source_thread_id.0 != c.session_id.as_str()
            && c.message.role == Role::User && c.message.id.is_agent_thread_report_of(&c.source_run_id)
            && a.delegation_origin.is_none() && a.inherited_toolsets().is_none()
            && a.input == [c.message.clone()],
    }
}
