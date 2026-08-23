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

pub(super) fn ensure_endpoint(reference: &TaskReference, endpoint: &str) -> Result<()> {
    if reference.endpoint == endpoint {
        Ok(())
    } else {
        Err(Error::Execution(format!(
            "durable A2A task belongs to endpoint {:?}, not {:?}",
            reference.endpoint, endpoint
        )))
    }
}
