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

/// The original coordinated Thread creation metadata, shared with committed
/// callback recovery. This derives no permission and creates no relationship.
pub fn coordinated_run_origin(
    session: &str,
    operation: &str,
    parent_run_id: awaken_agent_contract::RunId,
    parent_call_id: String,
    ancestor_agent: String,
) -> awaken_agent_contract::agent::delegation::DelegationOrigin {
    awaken_agent_contract::agent::delegation::DelegationOrigin {
        delegation_id: coordinated_origin_id(session, operation),
        parent_run_id,
        parent_call_id,
        depth: 1,
        agent_lineage: vec![ancestor_agent],
    }
}

/// Freeze the same existing activation after its Session-owned projection.
pub fn coordinated_run_activation(
    command: CoordinatedRunCommand,
    ancestor_agent: String,
    caller_toolsets: Vec<awaken_agent_contract::toolset::ToolsetPolicy>,
) -> awaken_runtime_contract::activation::RunActivation {
    if let (Some(source), Some(task)) = (command.task_attention_execution, &command.task_attention)
    {
        let mut activation = source.0;
        activation.run_id = command.run_id;
        activation.thread_id = command.thread_id;
        activation.input = task.messages.clone();
        return activation;
    }
    let input = coordinated_activation_input(
        command.session_id.as_str(),
        &command.thread_id,
        &command.run_id,
        &command.operation_id,
        command.message,
    );
    let origin = coordinated_run_origin(
        command.session_id.as_str(),
        &command.operation_id,
        command.parent_run_id,
        command.parent_call_id,
        ancestor_agent,
    );
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
    if let CoordinatedAdmissionRef::Child(c) = command
        && let Some(task) = &c.task_attention
    {
        let Some(source) = &c.task_attention_execution else {
            return false;
        };
        let mut expected = source.0.clone();
        expected.run_id = c.run_id.clone();
        expected.thread_id = c.thread_id.clone();
        expected.input = task.messages.clone();
        let Ok(expected) =
            expected.narrow_tools(task.execution_requirements.tool_capability_narrowing)
        else {
            return false;
        };
        return task
            .private_task_input
            .as_ref()
            .and_then(|input| input.tenure())
            .is_some_and(|tenure| {
                tenure.thread == c.thread_id.0
                    && tenure.session_thread() == c.session_id.as_str()
                    && task.session_id == c.session_id
                    && task.run_id == c.run_id
                    && task.agent_id == a.snapshot.root_agent_id.0
                    && &expected == a
            });
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
