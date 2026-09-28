// @formal entrypoint remote_attempt.cancel_addresses_pinned_task
// @formal entrypoint remote_attempt.committed_task_is_endpoint_pinned
// @formal entrypoint remote_attempt.endpoint_mismatch_fails_closed
// @formal entrypoint remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay
// @formal entrypoint remote_attempt.recovery_reattaches_without_resending
// @formal entrypoint remote_attempt.resume_preserves_context_and_task_reference
// @formal entrypoint remote_attempt.terminal_commit_retires_task_reference
//! Durable remote-operation identity and task binding for one A2A Run.

use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, MergePolicy, Scope, StateCell,
};
use awaken_protocol_a2a::Task;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result};
use awaken_runtime_contract::resolved::{Backend, ResolvedModelCandidate};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

const A2A_TASK_STATE_KEY: &str = "__a2a_task";
const A2A_REMOTE_OPERATION_SCHEMA: u8 = 2;
const A2A_REMOTE_OPERATION_ID_DOMAIN: &str = "a2a-remote-operation-v1";
const A2A_REMOTE_INPUT_ID_DOMAIN: &str = "a2a-remote-input-v1";

/// The opaque task identity bound to a previously prepared operation after
/// `message:send` returns. It is Run-scoped state, so a replacement Worker can
/// reattach without sending a second User message and cancellation can address
/// the same remote task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskReference {
    pub(super) endpoint: String,
    pub(super) task_id: String,
    pub(super) context_id: String,
}

/// The exact outbound operation committed before `message:send`.
///
/// A prepared operation deliberately does not claim that the peer accepted the
/// request. After a crash there is no safe way to infer that fact from the
/// predecessor task or from the stable message id alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteOperationIntent {
    pub(super) operation_id: String,
    pub(super) endpoint: String,
    pub(super) context_id: String,
    pub(super) message_id: String,
    pub(super) input_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) predecessor_task_id: Option<String>,
}

/// A prepared operation bound to the exact task returned by the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AcceptedRemoteOperation {
    pub(super) intent: RemoteOperationIntent,
    pub(super) reference: TaskReference,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedRemoteOperation {
    Prepared {
        version: u8,
        intent: RemoteOperationIntent,
    },
    Accepted {
        version: u8,
        intent: RemoteOperationIntent,
        reference: TaskReference,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteOperationState {
    Prepared(RemoteOperationIntent),
    Accepted(AcceptedRemoteOperation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteOperationRoute {
    Fresh,
    Prepared(RemoteOperationIntent),
    Reattach(AcceptedRemoteOperation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteOperationDecision {
    Fresh,
    PreservePrepared,
    Reattach,
    RejectEndpointMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredRemoteOperationKind {
    Absent,
    Prepared,
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteOperationSequencePhase {
    Fresh,
    PreparedCommitted,
    SendReturned,
    AcceptedCommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteOperationSequenceEvent {
    CommitPrepared,
    Send,
    CommitAccepted,
}

/// The sole outbound A2A sequencing plan. The async executor performs each
/// effect only after this production transition admits its event; initial and
/// resume sends therefore cannot grow separate ordering policies.
pub(super) const fn remote_operation_sequence_step(
    phase: RemoteOperationSequencePhase,
    event: RemoteOperationSequenceEvent,
) -> Option<RemoteOperationSequencePhase> {
    match (phase, event) {
        (RemoteOperationSequencePhase::Fresh, RemoteOperationSequenceEvent::CommitPrepared) => {
            Some(RemoteOperationSequencePhase::PreparedCommitted)
        }
        (RemoteOperationSequencePhase::PreparedCommitted, RemoteOperationSequenceEvent::Send) => {
            Some(RemoteOperationSequencePhase::SendReturned)
        }
        (
            RemoteOperationSequencePhase::SendReturned,
            RemoteOperationSequenceEvent::CommitAccepted,
        ) => Some(RemoteOperationSequencePhase::AcceptedCommitted),
        _ => None,
    }
}

#[must_use]
const fn task_reference_admitted(
    endpoint_present: bool,
    task_id_present: bool,
    context_id_present: bool,
    task_id_matches: bool,
    continuation_context_matches: bool,
) -> bool {
    endpoint_present
        && task_id_present
        && context_id_present
        && task_id_matches
        && continuation_context_matches
}

pub(super) const fn remote_operation_decision(
    kind: StoredRemoteOperationKind,
    endpoint_matches: bool,
) -> RemoteOperationDecision {
    match (kind, endpoint_matches) {
        (StoredRemoteOperationKind::Absent, _) => RemoteOperationDecision::Fresh,
        (StoredRemoteOperationKind::Prepared, true) => RemoteOperationDecision::PreservePrepared,
        (StoredRemoteOperationKind::Accepted, true) => RemoteOperationDecision::Reattach,
        (StoredRemoteOperationKind::Prepared | StoredRemoteOperationKind::Accepted, false) => {
            RemoteOperationDecision::RejectEndpointMismatch
        }
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

    fn validate(&self) -> Result<()> {
        if task_reference_admitted(
            !self.endpoint.is_empty(),
            !self.task_id.is_empty(),
            !self.context_id.is_empty(),
            true,
            true,
        ) {
            return Ok(());
        }
        let missing = [
            ("endpoint", self.endpoint.as_str()),
            ("task_id", self.task_id.as_str()),
            ("context_id", self.context_id.as_str()),
        ]
        .into_iter()
        .find(|(_, value)| value.is_empty())
        .map(|(name, _)| name);
        Err(Error::Execution(format!(
            "durable A2A task is missing {}",
            missing.expect("admission rejects only an absent reference field")
        )))
    }
}

impl RemoteOperationIntent {
    fn validate(&self) -> Result<()> {
        let missing = [
            ("operation_id", self.operation_id.as_str()),
            ("endpoint", self.endpoint.as_str()),
            ("context_id", self.context_id.as_str()),
            ("message_id", self.message_id.as_str()),
            ("input_digest", self.input_digest.as_str()),
        ]
        .into_iter()
        .find(|(_, value)| value.is_empty())
        .map(|(name, _)| name);
        if let Some(missing) = missing {
            return Err(Error::Execution(format!(
                "durable A2A operation is missing {missing}"
            )));
        }
        if self
            .predecessor_task_id
            .as_ref()
            .is_some_and(String::is_empty)
        {
            return Err(Error::Execution(
                "durable A2A operation has an empty predecessor task".to_string(),
            ));
        }
        let expected = remote_operation_id(
            &self.endpoint,
            &self.context_id,
            self.predecessor_task_id.as_deref(),
            &self.message_id,
            &self.input_digest,
        )?;
        if self.operation_id != expected {
            return Err(Error::Execution(
                "durable A2A operation identity does not match its coordinates".to_string(),
            ));
        }
        Ok(())
    }
}

impl AcceptedRemoteOperation {
    fn validate(&self) -> Result<()> {
        self.reference.validate()?;
        let intent = &self.intent;
        intent.validate()?;
        if intent.endpoint != self.reference.endpoint {
            return Err(Error::Execution(
                "accepted A2A task is not bound to its prepared endpoint".to_string(),
            ));
        }
        if intent.predecessor_task_id.is_some() && intent.context_id != self.reference.context_id {
            return Err(Error::Execution(format!(
                "A2A continuation changed context from {:?} to {:?}",
                intent.context_id, self.reference.context_id
            )));
        }
        Ok(())
    }
}

fn fingerprint(value: &impl serde::Serialize, what: &str) -> Result<String> {
    awaken_runtime_contract::resolution::content_fingerprint(value)
        .map_err(|error| Error::Execution(format!("failed to fingerprint A2A {what}: {error}")))
}

fn remote_operation_id(
    endpoint: &str,
    context_id: &str,
    predecessor_task_id: Option<&str>,
    message_id: &str,
    input_digest: &str,
) -> Result<String> {
    fingerprint(
        &(
            A2A_REMOTE_OPERATION_ID_DOMAIN,
            endpoint,
            context_id,
            predecessor_task_id,
            message_id,
            input_digest,
        ),
        "operation identity",
    )
}

pub(super) fn prepare_remote_operation(
    endpoint: &str,
    context_id: &str,
    predecessor_task_id: Option<&str>,
    message_id: &str,
    input: &str,
) -> Result<RemoteOperationIntent> {
    let input_digest = fingerprint(&(A2A_REMOTE_INPUT_ID_DOMAIN, input), "input")?;
    let intent = RemoteOperationIntent {
        operation_id: remote_operation_id(
            endpoint,
            context_id,
            predecessor_task_id,
            message_id,
            &input_digest,
        )?,
        endpoint: endpoint.to_string(),
        context_id: context_id.to_string(),
        message_id: message_id.to_string(),
        input_digest,
        predecessor_task_id: predecessor_task_id.map(str::to_string),
    };
    intent.validate()?;
    Ok(intent)
}

pub(super) fn accept_remote_operation(
    intent: RemoteOperationIntent,
    task: &Task,
) -> Result<AcceptedRemoteOperation> {
    let accepted = AcceptedRemoteOperation {
        reference: TaskReference::from_task(&intent.endpoint, task),
        intent,
    };
    accepted.validate()?;
    Ok(accepted)
}

pub(super) fn validate_reattached_task(reference: &TaskReference, task: &Task) -> Result<()> {
    if task_reference_admitted(
        !reference.endpoint.is_empty(),
        !task.id.is_empty(),
        !task.context_id.is_empty(),
        task.id == reference.task_id,
        task.context_id == reference.context_id,
    ) {
        return Ok(());
    }
    reference.validate()?;
    if task.id.is_empty() || task.context_id.is_empty() {
        return Err(Error::Execution(
            "reattached A2A task is missing its task or context identity".to_string(),
        ));
    }
    Err(Error::Execution(format!(
        "reattached A2A task identity changed from {:?}/{:?} to {:?}/{:?}",
        reference.task_id, reference.context_id, task.id, task.context_id
    )))
}

fn remote_operation_cell() -> StateCell<PersistedRemoteOperation> {
    StateCell::new(Scope::Run, MergePolicy::Disjoint, A2A_TASK_STATE_KEY)
}

pub(super) fn prepared_operation_state(intent: &RemoteOperationIntent) -> Result<StateCommand> {
    remote_operation_cell()
        .write(&PersistedRemoteOperation::Prepared {
            version: A2A_REMOTE_OPERATION_SCHEMA,
            intent: intent.clone(),
        })
        .map_err(|error| Error::Execution(error.to_string()))
}

pub(super) fn accepted_operation_state(accepted: &AcceptedRemoteOperation) -> Result<StateCommand> {
    accepted.validate()?;
    remote_operation_cell()
        .write(&PersistedRemoteOperation::Accepted {
            version: A2A_REMOTE_OPERATION_SCHEMA,
            intent: accepted.intent.clone(),
            reference: accepted.reference.clone(),
        })
        .map_err(|error| Error::Execution(error.to_string()))
}

pub(super) fn decode_remote_operation(value: &serde_json::Value) -> Result<RemoteOperationState> {
    let persisted = remote_operation_cell()
        .decode(value)
        .map_err(|error| Error::Execution(error.to_string()))?;
    let (version, state) = match persisted {
        PersistedRemoteOperation::Prepared { version, intent } => {
            intent.validate()?;
            (version, RemoteOperationState::Prepared(intent))
        }
        PersistedRemoteOperation::Accepted {
            version,
            intent,
            reference,
        } => {
            let accepted = AcceptedRemoteOperation { intent, reference };
            accepted.validate()?;
            (version, RemoteOperationState::Accepted(accepted))
        }
    };
    if version != A2A_REMOTE_OPERATION_SCHEMA {
        return Err(Error::Execution(format!(
            "unsupported durable A2A operation schema {version}"
        )));
    }
    Ok(state)
}

pub(super) fn clear_remote_operation_state() -> StateCommand {
    remote_operation_cell().remove()
}

#[cfg(test)]
pub(super) fn accepted_reference_state(reference: &TaskReference) -> Result<StateCommand> {
    let intent = prepare_remote_operation(
        &reference.endpoint,
        &reference.context_id,
        None,
        &format!("test-message:{}", reference.task_id),
        "test-input",
    )?;
    accepted_operation_state(&AcceptedRemoteOperation {
        intent,
        reference: reference.clone(),
    })
}

#[cfg(test)]
pub(super) fn restored_accepted_reference(
    context: &RuntimeRunContext,
    activation: &RunActivation,
) -> Result<Option<TaskReference>> {
    Ok(match restored_remote_operation(context, activation)? {
        Some(RemoteOperationState::Accepted(accepted)) => Some(accepted.reference),
        Some(RemoteOperationState::Prepared(_)) | None => None,
    })
}

pub(super) fn restored_remote_operation(
    context: &RuntimeRunContext,
    activation: &RunActivation,
) -> Result<Option<RemoteOperationState>> {
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
            StateAction::Set(value) => decode_remote_operation(&value).map(Some),
            StateAction::Remove => Ok(None),
        };
    }
    Ok(None)
}

pub(super) fn restored_remote_operation_route(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    endpoint: &str,
) -> Result<RemoteOperationRoute> {
    let operation = restored_remote_operation(context, activation)?;
    let (kind, persisted_endpoint) = match &operation {
        None => (StoredRemoteOperationKind::Absent, None),
        Some(RemoteOperationState::Prepared(intent)) => (
            StoredRemoteOperationKind::Prepared,
            Some(intent.endpoint.as_str()),
        ),
        Some(RemoteOperationState::Accepted(accepted)) => (
            StoredRemoteOperationKind::Accepted,
            Some(accepted.reference.endpoint.as_str()),
        ),
    };
    let decision = remote_operation_decision(kind, persisted_endpoint == Some(endpoint));
    match (decision, operation) {
        (RemoteOperationDecision::Fresh, None) => Ok(RemoteOperationRoute::Fresh),
        (
            RemoteOperationDecision::PreservePrepared,
            Some(RemoteOperationState::Prepared(intent)),
        ) => Ok(RemoteOperationRoute::Prepared(intent)),
        (RemoteOperationDecision::Reattach, Some(RemoteOperationState::Accepted(accepted))) => {
            Ok(RemoteOperationRoute::Reattach(accepted))
        }
        (RemoteOperationDecision::RejectEndpointMismatch, Some(operation)) => {
            let persisted_endpoint = match operation {
                RemoteOperationState::Prepared(intent) => intent.endpoint,
                RemoteOperationState::Accepted(accepted) => accepted.reference.endpoint,
            };
            Err(Error::Execution(format!(
                "durable A2A operation belongs to endpoint {:?}, not {:?}",
                persisted_endpoint, endpoint
            )))
        }
        _ => unreachable!("the total remote-operation decision preserves its input shape"),
    }
}

pub(super) fn remote_candidate_of(activation: &RunActivation) -> Result<&ResolvedModelCandidate> {
    let backend = activation.snapshot.resolved_spec.model_binding.backend();
    if !matches!(backend, Backend::Remote(_)) {
        return Err(Error::Execution(
            "A2A executor received a non-remote backend".to_string(),
        ));
    }
    Ok(&activation.snapshot.resolved_spec.model_binding)
}

pub(super) fn endpoint_of(candidate: &ResolvedModelCandidate) -> Result<String> {
    candidate
        .binding()
        .backend()
        .remote_endpoint()
        .map(str::to_string)
        .ok_or_else(|| Error::Execution("A2A executor received a non-remote backend".to_string()))
}

#[cfg(test)]
mod sequencing_tests {
    use super::*;

    #[test]
    fn outbound_operation_sequence_has_only_three_ordered_edges() {
        // Causes: current phase and requested durable/external event. Effects:
        // R1-R3 advance the unique Prepared-before-send plan; R4 samples the
        // critical forbidden fresh send and proves it stutters by rejection.
        //
        // | Rule | phase | event | result |
        // |---|---|---|---|
        // | R1 | Fresh | CommitPrepared | PreparedCommitted |
        // | R2 | PreparedCommitted | Send | SendReturned |
        // | R3 | SendReturned | CommitAccepted | AcceptedCommitted |
        // | R4 | Fresh | Send | reject |
        let prepared = remote_operation_sequence_step(
            RemoteOperationSequencePhase::Fresh,
            RemoteOperationSequenceEvent::CommitPrepared,
        );
        assert_eq!(
            prepared,
            Some(RemoteOperationSequencePhase::PreparedCommitted),
            "R1"
        );
        let sent =
            remote_operation_sequence_step(prepared.unwrap(), RemoteOperationSequenceEvent::Send);
        assert_eq!(sent, Some(RemoteOperationSequencePhase::SendReturned), "R2");
        assert_eq!(
            remote_operation_sequence_step(
                sent.unwrap(),
                RemoteOperationSequenceEvent::CommitAccepted,
            ),
            Some(RemoteOperationSequencePhase::AcceptedCommitted),
            "R3"
        );
        assert!(
            remote_operation_sequence_step(
                RemoteOperationSequencePhase::Fresh,
                RemoteOperationSequenceEvent::Send,
            )
            .is_none(),
            "R4"
        );
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// @obligation remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay
    #[kani::proof]
    fn remote_operation_sequence_admits_only_prepared_before_send_and_accept_commit() {
        // Causes: C1 current phase and C2 requested external/durable event.
        // Effects: E1 advance by exactly one edge or E2 reject. Only the three
        // listed edges exist; in particular Fresh+Send and replayed Send reject.
        //
        // | Rule | phase | event | Effect |
        // |---|---|---|---|
        // | R1 | Fresh | CommitPrepared | PreparedCommitted |
        // | R2 | PreparedCommitted | Send | SendReturned |
        // | R3 | SendReturned | CommitAccepted | AcceptedCommitted |
        // | R4 | every other pair | any | reject |
        let phase = match kani::any::<u8>() % 4 {
            0 => RemoteOperationSequencePhase::Fresh,
            1 => RemoteOperationSequencePhase::PreparedCommitted,
            2 => RemoteOperationSequencePhase::SendReturned,
            _ => RemoteOperationSequencePhase::AcceptedCommitted,
        };
        let event = match kani::any::<u8>() % 3 {
            0 => RemoteOperationSequenceEvent::CommitPrepared,
            1 => RemoteOperationSequenceEvent::Send,
            _ => RemoteOperationSequenceEvent::CommitAccepted,
        };
        let next = remote_operation_sequence_step(phase, event);
        let legal = matches!(
            (phase, event),
            (
                RemoteOperationSequencePhase::Fresh,
                RemoteOperationSequenceEvent::CommitPrepared
            ) | (
                RemoteOperationSequencePhase::PreparedCommitted,
                RemoteOperationSequenceEvent::Send
            ) | (
                RemoteOperationSequencePhase::SendReturned,
                RemoteOperationSequenceEvent::CommitAccepted
            )
        );
        assert_eq!(next.is_some(), legal);
        assert!(
            remote_operation_sequence_step(
                RemoteOperationSequencePhase::Fresh,
                RemoteOperationSequenceEvent::Send
            )
            .is_none()
        );
    }

    /// @obligation remote_attempt.committed_task_is_endpoint_pinned
    /// @obligation remote_attempt.endpoint_mismatch_fails_closed
    /// @obligation remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay
    /// @obligation remote_attempt.recovery_reattaches_without_resending
    #[kani::proof]
    fn remote_operation_route_is_total_and_fail_closed() {
        // Cause graph: C1 state is absent/prepared/accepted; C2 its endpoint
        // matches the publication-frozen endpoint. Effects: E1 fresh send is
        // admitted; E2 a prepared operation is preserved without outbound IO;
        // E3 an accepted task may reattach; E4 mismatch is rejected.
        // Constraint: C2 has no authority when state is absent.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | R1 | absent | *   | E1 |
        // | R2 | prepared | yes | E2 |
        // | R3 | accepted | yes | E3 |
        // | R4 | prepared/accepted | no | E4 |
        let selector: u8 = kani::any();
        let kind = match selector % 3 {
            0 => StoredRemoteOperationKind::Absent,
            1 => StoredRemoteOperationKind::Prepared,
            _ => StoredRemoteOperationKind::Accepted,
        };
        let endpoint_matches: bool = kani::any();
        let decision = remote_operation_decision(kind, endpoint_matches);

        assert_eq!(
            decision == RemoteOperationDecision::Fresh,
            kind == StoredRemoteOperationKind::Absent
        );
        assert_eq!(
            decision == RemoteOperationDecision::PreservePrepared,
            kind == StoredRemoteOperationKind::Prepared && endpoint_matches
        );
        assert_eq!(
            decision == RemoteOperationDecision::Reattach,
            kind == StoredRemoteOperationKind::Accepted && endpoint_matches
        );
        assert_eq!(
            decision == RemoteOperationDecision::RejectEndpointMismatch,
            kind != StoredRemoteOperationKind::Absent && !endpoint_matches
        );
    }

    /// @obligation remote_attempt.terminal_commit_retires_task_reference
    #[kani::proof]
    fn terminal_remote_operation_command_is_exact_run_scoped_removal() {
        // Cause graph: C1 a Run reaches a committed terminal boundary.
        // Effect E1 the sole A2A task-reference key is removed in Run scope
        // under its declared Disjoint merge policy; no Set or second key is
        // admitted. There is one unconditional rule C1 -> E1, so a multi-row
        // decision table would add no information.
        let command = clear_remote_operation_state();

        assert_eq!(command.scope, Scope::Run);
        assert_eq!(command.merge, MergePolicy::Disjoint);
        assert_eq!(command.key.0, A2A_TASK_STATE_KEY);
        assert!(command.run_id.is_none());
        assert!(matches!(command.action, StateAction::Remove));
    }

    /// @obligation remote_attempt.cancel_addresses_pinned_task
    /// @obligation remote_attempt.resume_preserves_context_and_task_reference
    #[kani::proof]
    fn task_reference_admission_requires_complete_fields_and_required_identity_matches() {
        // Causes: C1 endpoint present; C2 task id present; C3 context id
        // present; C4 task identity matches when reattaching; C5 continuation
        // context matches the committed reference.
        // Effect E1 admits replacement of the durable task reference; E2
        // rejects it without replacing committed state.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // |---|---|---|---|---|---|---|
        // | R1 | yes | yes | yes | yes | yes | E1 |
        // | R2 | no  | * | * | * | * | E2 |
        // | R3 | * | no  | * | * | * | E2 |
        // | R4 | * | * | no  | * | * | E2 |
        // | R5 | * | * | * | no  | * | E2 |
        // | R6 | * | * | * | * | no  | E2 |
        let endpoint_present: bool = kani::any();
        let task_id_present: bool = kani::any();
        let context_id_present: bool = kani::any();
        let task_id_matches: bool = kani::any();
        let continuation_context_matches: bool = kani::any();

        assert_eq!(
            task_reference_admitted(
                endpoint_present,
                task_id_present,
                context_id_present,
                task_id_matches,
                continuation_context_matches,
            ),
            endpoint_present
                && task_id_present
                && context_id_present
                && task_id_matches
                && continuation_context_matches
        );
    }
}
