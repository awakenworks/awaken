//! Durable remote-task identity and endpoint binding for one A2A Run.

use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, MergePolicy, Scope, StateCell,
};
use awaken_protocol_a2a::Task;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result};
use awaken_runtime_contract::resolved::{Backend, ResolvedModelCandidate};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

const A2A_TASK_STATE_KEY: &str = "__a2a_task";

/// The opaque remote identity committed immediately after `message:send`
/// returns. It is Run-scoped state, so a replacement Worker can reattach
/// without sending a second User message and cancellation can address the same
/// remote task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskReference {
    pub(super) endpoint: String,
    pub(super) task_id: String,
    pub(super) context_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TaskReferenceRoute {
    Fresh,
    Reattach(TaskReference),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskReferenceDecision {
    Fresh,
    Reattach,
    RejectEndpointMismatch,
}

pub(super) fn task_reference_decision(
    has_reference: bool,
    endpoint_matches: bool,
) -> TaskReferenceDecision {
    match (has_reference, endpoint_matches) {
        (false, _) => TaskReferenceDecision::Fresh,
        (true, true) => TaskReferenceDecision::Reattach,
        (true, false) => TaskReferenceDecision::RejectEndpointMismatch,
    }
}

impl TaskReference {
    pub(super) fn from_task(endpoint: &str, task: &Task) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
        }
    }

    fn validate(self) -> Result<Self> {
        let missing = [
            ("endpoint", self.endpoint.as_str()),
            ("task_id", self.task_id.as_str()),
            ("context_id", self.context_id.as_str()),
        ]
        .into_iter()
        .find(|(_, value)| value.is_empty())
        .map(|(name, _)| name);
        match missing {
            Some(name) => Err(Error::Execution(format!(
                "durable A2A task is missing {name}"
            ))),
            None => Ok(self),
        }
    }
}

fn task_reference_cell() -> StateCell<TaskReference> {
    StateCell::new(Scope::Run, MergePolicy::Disjoint, A2A_TASK_STATE_KEY)
}

pub(super) fn task_reference_state(reference: &TaskReference) -> Result<StateCommand> {
    task_reference_cell()
        .write(reference)
        .map_err(|error| Error::Execution(error.to_string()))
}

pub(super) fn decode_task_reference(value: &serde_json::Value) -> Result<TaskReference> {
    task_reference_cell()
        .decode(value)
        .map_err(|error| Error::Execution(error.to_string()))?
        .validate()
}

pub(super) fn clear_task_reference_state() -> StateCommand {
    task_reference_cell().remove()
}

pub(super) fn restored_task_reference(
    context: &RuntimeRunContext,
    activation: &RunActivation,
) -> Result<Option<TaskReference>> {
    let Some(reader) = &context.reader else {
        return Ok(None);
    };
    for command in reader
        .committed_state(&activation.thread_id)
        .into_iter()
        .rev()
    {
        if command.scope != Scope::Run
            || command.run_id.as_ref() != Some(&activation.run_id)
            || command.key.0 != A2A_TASK_STATE_KEY
        {
            continue;
        }
        return match command.action {
            StateAction::Set(value) => decode_task_reference(&value).map(Some),
            StateAction::Remove => Ok(None),
        };
    }
    Ok(None)
}

pub(super) fn restored_task_route(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    endpoint: &str,
) -> Result<TaskReferenceRoute> {
    let reference = restored_task_reference(context, activation)?;
    let decision = task_reference_decision(
        reference.is_some(),
        reference
            .as_ref()
            .is_some_and(|reference| reference.endpoint == endpoint),
    );
    match (decision, reference) {
        (TaskReferenceDecision::Fresh, None) => Ok(TaskReferenceRoute::Fresh),
        (TaskReferenceDecision::Reattach, Some(reference)) => {
            Ok(TaskReferenceRoute::Reattach(reference))
        }
        (TaskReferenceDecision::RejectEndpointMismatch, Some(reference)) => {
            Err(Error::Execution(format!(
                "durable A2A task belongs to endpoint {:?}, not {:?}",
                reference.endpoint, endpoint
            )))
        }
        _ => unreachable!("the total task-reference decision preserves its input shape"),
    }
}

pub(super) fn remote_candidate_of(activation: &RunActivation) -> Result<&ResolvedModelCandidate> {
    let backend = Backend::from_ref(
        &activation
            .snapshot
            .resolved_spec
            .model_binding
            .binding()
            .backend_ref,
    );
    if !matches!(backend, Backend::Remote(_)) {
        return Err(Error::Execution(
            "A2A executor received a non-remote backend".to_string(),
        ));
    }
    Ok(&activation.snapshot.resolved_spec.model_binding)
}

pub(super) fn endpoint_of(candidate: &ResolvedModelCandidate) -> Result<String> {
    Backend::from_ref(&candidate.binding().backend_ref)
        .remote_endpoint()
        .map(str::to_string)
        .ok_or_else(|| Error::Execution("A2A executor received a non-remote backend".to_string()))
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn remote_task_reference_route_is_total_and_fail_closed() {
        // Cause graph: C1 a committed reference is present; C2 its endpoint
        // matches the publication-frozen endpoint. Effects: E1 fresh send is
        // admitted; E2 reattachment is admitted; E3 outbound use is rejected.
        // Constraint: C2 has no authority when C1 is false.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | R1 | no  | *   | E1 |
        // | R2 | yes | yes | E2 |
        // | R3 | yes | no  | E3 |
        let has_reference: bool = kani::any();
        let endpoint_matches: bool = kani::any();
        let decision = task_reference_decision(has_reference, endpoint_matches);

        assert_eq!(decision == TaskReferenceDecision::Fresh, !has_reference);
        assert_eq!(
            decision == TaskReferenceDecision::Reattach,
            has_reference && endpoint_matches
        );
        assert_eq!(
            decision == TaskReferenceDecision::RejectEndpointMismatch,
            has_reference && !endpoint_matches
        );
    }

    #[kani::proof]
    fn terminal_task_reference_command_is_exact_run_scoped_removal() {
        // Cause graph: C1 a Run reaches a committed terminal boundary.
        // Effect E1 the sole A2A task-reference key is removed in Run scope
        // under its declared Disjoint merge policy; no Set or second key is
        // admitted. There is one unconditional rule C1 -> E1, so a multi-row
        // decision table would add no information.
        let command = clear_task_reference_state();

        assert_eq!(command.scope, Scope::Run);
        assert_eq!(command.merge, MergePolicy::Disjoint);
        assert_eq!(command.key.0, A2A_TASK_STATE_KEY);
        assert!(command.run_id.is_none());
        assert!(matches!(command.action, StateAction::Remove));
    }
}
