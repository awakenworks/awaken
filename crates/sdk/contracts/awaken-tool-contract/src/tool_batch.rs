// @formal entrypoint tool.executor_entry_order
// @formal entrypoint tool.publication_barrier
// @formal entrypoint tool.recovery_policy
// @formal entrypoint tool.session_thread_composition
// @formal entrypoint tool.transition_preconditions
// @formal entrypoint tool_result.call_is_consumed_at_most_once
//! Durable state of one model-emitted tool-call batch.
//!
//! The transcript is exposed to the model only after every call has a terminal
//! result, but each call's execution outcome is committed independently. This
//! cell is the recovery truth; futures, abort handles, and worker tasks are not.

use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, ResumeTicket, ToolAwaitReason, ToolCallAuthority,
};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, MergePolicy, Scope, StateKey, Store,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::num::NonZeroU16;

mod execution_domain;
mod result_identity;
mod source;
pub use execution_domain::{
    PendingToolExecutionEntry, ResourceOriginRef, ToolExecutionDomain, ToolExecutionDomainFootprint,
};
pub use source::store_from_run_commands;

use crate::tool::{
    ToolCall, ToolOutput, ToolRecoveryCapability, ToolRecoveryMode, ToolRecoveryPolicy,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBatchId(String);

impl ToolBatchId {
    #[must_use]
    fn for_step(run_id: &RunId, step: usize) -> Self {
        Self(format!("tool-batch:{}:{step}", run_id.0))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn belongs_to(&self, run_id: &RunId) -> bool {
        self.0
            .strip_prefix("tool-batch:")
            .and_then(|value| value.rsplit_once(':'))
            .is_some_and(|(owner, step)| owner == run_id.0.as_str() && step.parse::<u64>().is_ok())
    }

    fn operation_id(&self, call_id: &str) -> String {
        format!("{}:{call_id}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolCallPhase {
    /// The request is durable but no executor has been entered.
    Requested,
    /// The executor may have produced an external effect. A crash here must use
    /// the pinned recovery policy; it must never silently restart as Requested.
    Executing { attempt: u16 },
    /// A durable wait (permission/delegation/external result) owns this correlation.
    Awaiting { wait: ToolWait },
    /// Exact physical return, durable before materialization or phase hooks.
    /// Its state is data until the Runtime folds calls in model order.
    Returned(ToolOutput),
    /// A known physical return whose owning Run ended before publication.
    /// This is not an unknown effect and never fabricates an error ToolResult.
    KnownAborted { output: ToolOutput, reason: String },
    /// Terminal materialized output committed before transcript publication.
    /// Legacy completed calls with empty result vectors remain valid.
    Completed(ToolOutput),
    /// The external outcome cannot safely be determined.
    Indeterminate { reason: String },
}

/// The typed reason one call is waiting. Tool permission is deliberately not
/// encoded as a generic message: it must match the Run's committed ResumeTicket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolWaitKind {
    #[serde(alias = "Approval")]
    ToolPermission,
    Delegation,
    ScheduledAction,
    ExternalResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolWait {
    pub kind: ToolWaitKind,
    pub correlation_id: String,
}

impl ToolCallPhase {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::KnownAborted { .. } | Self::Indeterminate { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableToolCall {
    pub call: ToolCall,
    pub recovery_policy: ToolRecoveryPolicy,
    pub phase: ToolCallPhase,
    /// Pre-effect containment of the latest executor entry. Historical entries
    /// stay in ThreadCommit; absent legacy metadata proves no containment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_domain: Option<ToolExecutionDomain>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialization_domain: Option<ToolExecutionDomain>,
    /// Ordered model-visible messages for this call (the tool result followed by
    /// any durable reminders). Kept behind the batch publication barrier.
    #[serde(default)]
    pub result_messages: Vec<awaken_agent_contract::agent::message::Message>,
    /// Tool-owned state and reactions, kept durable but unpublished until the
    /// whole batch passes validation and crosses the publication barrier.
    #[serde(default)]
    pub result_state: Vec<awaken_agent_contract::agent::state::Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolBatchPhase {
    Open,
    Finalized,
}

/// The one phase decision for recovery of an unfinished call. A permitted
/// entry is borrowed and uncommitted until its actual domain is installed.
///
/// `Indeterminate` means the aggregate has already sealed the call with the
/// exact fail-closed reason. The caller only persists and publishes that state;
/// it must not maintain a parallel recovery receipt.
#[derive(Debug)]
pub enum ToolRecoveryAction<'a> {
    Entered {
        entry: PendingToolExecutionEntry<'a>,
    },
    Indeterminate,
}

/// Validated aggregate for a non-empty, uniquely identified tool-call batch.
/// Its storage is private so callers cannot insert duplicate calls, reopen a
/// finalized batch, or publish staged effects before a terminal result.
///
/// ```compile_fail
/// use awaken_tool_contract::tool_batch::{ToolBatch, ToolBatchPhase, ToolBatchId};
/// use awaken_agent_contract::agent::run::Id as RunId;
///
/// let _ = ToolBatch {
///     id: ToolBatchId("tool-batch:run:0".into()),
///     run_id: RunId("run".into()),
///     calls: Vec::new(),
///     phase: ToolBatchPhase::Open,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolBatch {
    id: ToolBatchId,
    run_id: RunId,
    calls: Vec<DurableToolCall>,
    phase: ToolBatchPhase,
}

/// Ordered staged effects exposed only after the owning batch is finalized.
///
/// The view borrows the canonical aggregate, so Runtime publication cannot
/// copy the staged data into a parallel receipt or observe it through a second
/// lifecycle owner.
pub struct ToolBatchPublication<'a> {
    calls: &'a [DurableToolCall],
}

/// Borrowed proof that one call in the canonical durable batch is in its
/// `Executing` phase. Runtime obtains this view only after the corresponding
/// `ThreadCommit` returns successfully, then consumes its operation identity at
/// the executor boundary. The private fields prevent callers from fabricating
/// an admission for a Requested, Awaiting, or terminal call.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionAdmission<'a> {
    batch_id: &'a ToolBatchId,
    call_id: &'a str,
    attempt: u16,
}

impl ToolExecutionAdmission<'_> {
    #[must_use]
    pub fn call_id(&self) -> &str {
        self.call_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u16 {
        self.attempt
    }

    #[must_use]
    pub fn operation_id(&self) -> String {
        self.batch_id.operation_id(self.call_id)
    }
}

impl ToolBatchPublication<'_> {
    /// Tool-owned state in the original model call order.
    pub fn result_state(&self) -> impl Iterator<Item = &StateCommand> {
        self.calls.iter().flat_map(|entry| &entry.result_state)
    }

    /// Model-visible result messages in the original model call order.
    pub fn messages(
        &self,
    ) -> impl Iterator<Item = &awaken_agent_contract::agent::message::Message> {
        self.calls.iter().flat_map(|entry| &entry.result_messages)
    }
}

#[derive(Deserialize)]
struct ToolBatchWire {
    id: ToolBatchId,
    run_id: RunId,
    calls: Vec<DurableToolCall>,
    phase: ToolBatchPhase,
}

impl<'de> Deserialize<'de> for ToolBatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ToolBatchWire::deserialize(deserializer)?;
        let batch = Self {
            id: wire.id,
            run_id: wire.run_id,
            calls: wire.calls,
            phase: wire.phase,
        };
        batch.validate().map_err(D::Error::custom)?;
        Ok(batch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolBatchError {
    #[error("tool batch run id must not be empty")]
    EmptyRunId,
    #[error("tool batch id does not belong to its run")]
    InvalidBatchId,
    #[error("tool batch must contain at least one call")]
    Empty,
    #[error("duplicate tool call id {0}")]
    DuplicateCall(String),
    #[error("unknown tool call id {0}")]
    UnknownCall(String),
    #[error("tool batch is already finalized")]
    Finalized,
    #[error("invalid tool-call transition")]
    InvalidTransition,
    #[error("tool-call attempt budget exhausted")]
    AttemptsExhausted,
    #[error("tool wait kind or correlation does not match")]
    WaitMismatch,
    #[error("tool batch may contain at most one awaiting call")]
    MultipleAwaitingCalls,
    #[error("tool wait correlation must not be empty")]
    EmptyWaitCorrelation,
    #[error("tool batch still contains non-terminal calls")]
    Incomplete,
    #[error("tool batch is not finalized for publication")]
    Unfinalized,
    #[error("persisted tool batch is invalid: {0}")]
    InvalidPersistedState(String),
}

/// Why a committed Run ticket and its Run-scoped tool batch do not describe the
/// same durable wait. This is the single cross-aggregate consistency check used
/// by resume, interruption, and public reply admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ToolBatchWaitError {
    #[error("tool batch is finalized")]
    Finalized,
    #[error("resume ticket belongs to another Run")]
    RunMismatch,
    #[error("resume ticket belongs to another Thread")]
    ThreadMismatch,
    #[error("resume ticket does not target a tool call")]
    NotToolCall,
    #[error("resume ticket tool call is owned by another execution authority")]
    AuthorityMismatch,
    #[error("tool batch has no awaiting call")]
    MissingAwaitingCall,
    #[error("resume ticket names another call")]
    CallMismatch,
    #[error("resume ticket has another wait kind")]
    KindMismatch,
    #[error("resume ticket has another correlation")]
    CorrelationMismatch,
    #[error("resume ticket has another tool payload")]
    ToolMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallPhaseView {
    Requested,
    Executing(u16),
    Awaiting(ToolWaitKind),
    Returned,
    KnownAborted,
    Completed,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionRequest {
    StartOrRetry,
    Resume {
        expected_kind: ToolWaitKind,
        correlation_matches: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallTransition {
    CompleteFromExecutionOrWait,
    CompleteImmediate,
    RecordReturn,
    MaterializeReturn,
    MarkIndeterminate,
    StageResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionDisposition {
    Apply,
    Replay,
    Reject,
}

/// Classify completion through the same phase guard used by the aggregate.
/// An exact terminal result is an inert replay; no terminal predecessor can
/// ever apply a second result to the durable call.
const fn completion_disposition(
    phase: CallPhaseView,
    transition: CallTransition,
    exact_completed_output: bool,
) -> CompletionDisposition {
    if matches!(phase, CallPhaseView::Completed) && exact_completed_output {
        CompletionDisposition::Replay
    } else if permits_call_transition(phase, transition) {
        CompletionDisposition::Apply
    } else {
        CompletionDisposition::Reject
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryTransition {
    Enter(u16),
    NeverReplay,
    UnsupportedCapability,
    AttemptsExhausted,
}

const fn permits_call_transition(phase: CallPhaseView, transition: CallTransition) -> bool {
    match transition {
        CallTransition::CompleteFromExecutionOrWait => matches!(
            phase,
            CallPhaseView::Executing(_) | CallPhaseView::Awaiting(_)
        ),
        CallTransition::CompleteImmediate => matches!(phase, CallPhaseView::Requested),
        CallTransition::RecordReturn => matches!(
            phase,
            CallPhaseView::Requested | CallPhaseView::Executing(_) | CallPhaseView::Awaiting(_)
        ),
        CallTransition::MaterializeReturn => matches!(phase, CallPhaseView::Returned),
        CallTransition::MarkIndeterminate => !matches!(
            phase,
            CallPhaseView::Returned | CallPhaseView::KnownAborted | CallPhaseView::Completed
        ),
        CallTransition::StageResult => matches!(
            phase,
            CallPhaseView::Completed | CallPhaseView::Indeterminate
        ),
    }
}

const fn permits_await_transition(phase: CallPhaseView, another_call_is_awaiting: bool) -> bool {
    !another_call_is_awaiting
        && matches!(
            phase,
            CallPhaseView::Requested | CallPhaseView::Executing(_)
        )
}

const fn tool_wait_kind(reason: ToolAwaitReason) -> ToolWaitKind {
    match reason {
        ToolAwaitReason::Permission => ToolWaitKind::ToolPermission,
        ToolAwaitReason::ClientExecution => ToolWaitKind::ExternalResult,
        ToolAwaitReason::ScheduledAction => ToolWaitKind::ScheduledAction,
        ToolAwaitReason::Delegation => ToolWaitKind::Delegation,
    }
}

const fn must_seal_on_run_end(phase: CallPhaseView) -> bool {
    !matches!(
        phase,
        CallPhaseView::KnownAborted | CallPhaseView::Completed | CallPhaseView::Indeterminate
    )
}

/// Heap-free production admission kernel shared by the aggregate publication
/// view and Kani. Completing every call is necessary but does not expose its
/// staged effects until `finalize` closes the batch.
const fn permits_publication(phase: ToolBatchPhase) -> bool {
    matches!(phase, ToolBatchPhase::Finalized)
}

fn phase_view(phase: &ToolCallPhase) -> CallPhaseView {
    match phase {
        ToolCallPhase::Requested => CallPhaseView::Requested,
        ToolCallPhase::Executing { attempt } => CallPhaseView::Executing(*attempt),
        ToolCallPhase::Awaiting { wait } => CallPhaseView::Awaiting(wait.kind),
        ToolCallPhase::Returned(_) => CallPhaseView::Returned,
        ToolCallPhase::KnownAborted { .. } => CallPhaseView::KnownAborted,
        ToolCallPhase::Completed(_) => CallPhaseView::Completed,
        ToolCallPhase::Indeterminate { .. } => CallPhaseView::Indeterminate,
    }
}

const fn executor_entry_attempt(phase: CallPhaseView) -> Option<u16> {
    match phase {
        CallPhaseView::Executing(attempt) => Some(attempt),
        CallPhaseView::Requested
        | CallPhaseView::Awaiting(_)
        | CallPhaseView::Returned
        | CallPhaseView::KnownAborted
        | CallPhaseView::Completed
        | CallPhaseView::Indeterminate => None,
    }
}

const FIRST_EXECUTION_ATTEMPT: u16 = 1;

/// Heap-free production transition kernel shared by the aggregate methods and
/// Kani. It is the single authority for entering an executor attempt.
fn next_execution_attempt(
    phase: CallPhaseView,
    max_attempts: NonZeroU16,
    request: ExecutionRequest,
) -> Result<u16, ToolBatchError> {
    let attempt = match (phase, request) {
        (CallPhaseView::Requested, ExecutionRequest::StartOrRetry) => FIRST_EXECUTION_ATTEMPT,
        (CallPhaseView::Executing(attempt), ExecutionRequest::StartOrRetry) => attempt
            .checked_add(1)
            .ok_or(ToolBatchError::AttemptsExhausted)?,
        (
            CallPhaseView::Awaiting(actual_kind),
            ExecutionRequest::Resume {
                expected_kind,
                correlation_matches: true,
            },
        ) if actual_kind == expected_kind => FIRST_EXECUTION_ATTEMPT,
        (CallPhaseView::Awaiting(_), ExecutionRequest::Resume { .. }) => {
            return Err(ToolBatchError::WaitMismatch);
        }
        _ => return Err(ToolBatchError::InvalidTransition),
    };
    if attempt > max_attempts.get() {
        Err(ToolBatchError::AttemptsExhausted)
    } else {
        Ok(attempt)
    }
}

/// Combine the pinned policy, trusted executable capability and durable attempt
/// into one recovery decision. `ToolBatch::recover_execution` is the sole
/// mutating consumer; keeping this kernel heap-free lets Kani cover the full
/// enum and integer domains without inventing a second recovery state machine.
fn recovery_transition(
    phase: CallPhaseView,
    policy: &ToolRecoveryPolicy,
    capability: ToolRecoveryCapability,
) -> Result<RecoveryTransition, ToolBatchError> {
    if !matches!(
        phase,
        CallPhaseView::Requested | CallPhaseView::Executing(_)
    ) {
        return Err(ToolBatchError::InvalidTransition);
    }
    if matches!(phase, CallPhaseView::Executing(_))
        && policy.mode() == ToolRecoveryMode::NeverReplay
    {
        return Ok(RecoveryTransition::NeverReplay);
    }
    if !policy.mode().is_supported_by(capability) {
        return Ok(RecoveryTransition::UnsupportedCapability);
    }
    match next_execution_attempt(phase, policy.max_attempts(), ExecutionRequest::StartOrRetry) {
        Ok(attempt) => Ok(RecoveryTransition::Enter(attempt)),
        Err(ToolBatchError::AttemptsExhausted) => Ok(RecoveryTransition::AttemptsExhausted),
        Err(error) => Err(error),
    }
}

impl ToolBatch {
    /// Reconstruct the canonical operation identity for one call from its
    /// durable Run/step coordinate without exposing a constructible batch id.
    #[must_use]
    pub fn operation_id_for_step(run_id: &RunId, step: usize, call_id: &str) -> String {
        ToolBatchId::for_step(run_id, step).operation_id(call_id)
    }

    pub fn for_step(
        run_id: RunId,
        step: usize,
        calls: impl IntoIterator<Item = (ToolCall, ToolRecoveryPolicy)>,
    ) -> Result<Self, ToolBatchError> {
        if run_id.0.trim().is_empty() {
            return Err(ToolBatchError::EmptyRunId);
        }
        let id = ToolBatchId::for_step(&run_id, step);
        let calls: Vec<_> = calls
            .into_iter()
            .map(|(call, recovery_policy)| DurableToolCall {
                call,
                recovery_policy,
                execution_domain: None,
                materialization_domain: None,
                phase: ToolCallPhase::Requested,
                result_messages: Vec::new(),
                result_state: Vec::new(),
            })
            .collect();
        if calls.is_empty() {
            return Err(ToolBatchError::Empty);
        }
        let mut ids = std::collections::BTreeSet::new();
        for call in &calls {
            if !ids.insert(call.call.call_id.clone()) {
                return Err(ToolBatchError::DuplicateCall(call.call.call_id.clone()));
            }
        }
        Ok(Self {
            id,
            run_id,
            calls,
            phase: ToolBatchPhase::Open,
        })
    }

    #[must_use]
    pub fn id(&self) -> &ToolBatchId {
        &self.id
    }

    /// Canonical identity of one call already owned by this durable batch.
    #[must_use]
    pub fn operation_id(&self, call_id: &str) -> String {
        self.id.operation_id(call_id)
    }

    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    #[must_use]
    pub fn calls(&self) -> &[DurableToolCall] {
        &self.calls
    }

    #[must_use]
    pub const fn phase(&self) -> ToolBatchPhase {
        self.phase
    }

    /// Project the exact executor-entry capability from this aggregate.
    ///
    /// This does not persist the transition. Runtime first commits the mutated
    /// batch, then asks the same value for this borrowed proof immediately
    /// before invoking the executor.
    pub fn execution_admission(
        &self,
        call_id: &str,
    ) -> Result<ToolExecutionAdmission<'_>, ToolBatchError> {
        let call = self
            .calls
            .iter()
            .find(|entry| entry.call.call_id == call_id)
            .ok_or_else(|| ToolBatchError::UnknownCall(call_id.to_string()))?;
        let attempt = executor_entry_attempt(phase_view(&call.phase))
            .ok_or(ToolBatchError::InvalidTransition)?;
        Ok(ToolExecutionAdmission {
            batch_id: &self.id,
            call_id: &call.call.call_id,
            attempt,
        })
    }

    /// Validate the complete persisted aggregate before recovery can act on it.
    /// Custom deserialization calls this automatically, so corrupted indexes,
    /// attempts, waits, or publication state fail closed at the state boundary.
    pub fn validate(&self) -> Result<(), ToolBatchError> {
        if self.run_id.0.trim().is_empty() {
            return Err(ToolBatchError::EmptyRunId);
        }
        if !self.id.belongs_to(&self.run_id) {
            return Err(ToolBatchError::InvalidBatchId);
        }
        if self.calls.is_empty() {
            return Err(ToolBatchError::Empty);
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut awaiting_calls = 0_u8;
        for entry in &self.calls {
            if let Some(domain) = &entry.execution_domain {
                domain.validate()?;
            }
            if let Some(domain) = &entry.materialization_domain {
                domain.validate()?;
            }
            let call_id = &entry.call.call_id;
            if !ids.insert(call_id) {
                return Err(ToolBatchError::DuplicateCall(call_id.clone()));
            }
            match &entry.phase {
                ToolCallPhase::Executing { attempt }
                    if *attempt == 0 || *attempt > entry.recovery_policy.max_attempts().get() =>
                {
                    return Err(ToolBatchError::InvalidPersistedState(format!(
                        "call {call_id} has an out-of-budget execution attempt"
                    )));
                }
                ToolCallPhase::Awaiting { wait } => {
                    awaiting_calls = awaiting_calls.saturating_add(1);
                    if wait.correlation_id.is_empty() {
                        return Err(ToolBatchError::EmptyWaitCorrelation);
                    }
                }
                ToolCallPhase::Returned(output)
                | ToolCallPhase::KnownAborted { output, .. }
                | ToolCallPhase::Completed(output)
                    if output.call_id != *call_id =>
                {
                    return Err(ToolBatchError::InvalidPersistedState(format!(
                        "call {call_id} contains another call's output"
                    )));
                }
                _ => {}
            }
            if matches!(entry.phase, ToolCallPhase::KnownAborted { .. })
                && (!entry.result_messages.is_empty() || !entry.result_state.is_empty())
            {
                return Err(ToolBatchError::InvalidPersistedState(
                    "an aborted known return cannot publish effects".into(),
                ));
            }
            if (!entry.result_messages.is_empty() || !entry.result_state.is_empty())
                && !entry.phase.is_terminal()
                && !matches!(&entry.phase, ToolCallPhase::Returned(_)
                    if entry.result_state.is_empty()
                        && entry.result_messages.iter().all(|message|
                            message.role == awaken_agent_contract::agent::message::Role::System))
            {
                return Err(ToolBatchError::InvalidPersistedState(format!(
                    "call {call_id} publishes staged effects before a terminal result"
                )));
            }
        }
        if awaiting_calls > 1 {
            return Err(ToolBatchError::MultipleAwaitingCalls);
        }
        if self.phase == ToolBatchPhase::Finalized && !self.is_complete() {
            return Err(ToolBatchError::InvalidPersistedState(
                "a finalized batch contains a non-terminal call".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.calls.iter().all(|call| call.phase.is_terminal())
    }

    /// Apply the complete recovery policy to one unfinished durable call.
    ///
    /// A never-entered `Requested` call may make its first attempt even under
    /// `NeverReplay`. Once an `Executing` call is recovered, NeverReplay,
    /// unsupported capability and an exhausted attempt budget all seal it as
    /// `Indeterminate`; only a supported policy with remaining budget advances
    /// to the exact next attempt. A permitted entry is staged without mutation
    /// so domain preparation happens after this decision and before persistence.
    pub fn recover_execution(
        &mut self,
        call_id: &str,
        capability: ToolRecoveryCapability,
    ) -> Result<ToolRecoveryAction<'_>, ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let transition =
            recovery_transition(phase_view(&call.phase), &call.recovery_policy, capability)?;
        match transition {
            RecoveryTransition::Enter(attempt) => Ok(ToolRecoveryAction::Entered {
                entry: PendingToolExecutionEntry::new(call, attempt),
            }),
            RecoveryTransition::NeverReplay => {
                call.phase = ToolCallPhase::Indeterminate {
                    reason: "owner lease expired after the executor was entered".into(),
                };
                Ok(ToolRecoveryAction::Indeterminate)
            }
            RecoveryTransition::UnsupportedCapability => {
                let reason = call
                    .recovery_policy
                    .validate(capability)
                    .expect_err("the recovery kernel rejected this capability")
                    .to_string();
                call.phase = ToolCallPhase::Indeterminate { reason };
                Ok(ToolRecoveryAction::Indeterminate)
            }
            RecoveryTransition::AttemptsExhausted => {
                call.phase = ToolCallPhase::Indeterminate {
                    reason: "recovery attempt budget exhausted".into(),
                };
                Ok(ToolRecoveryAction::Indeterminate)
            }
        }
    }

    /// Resume a typed durable wait into execution. Both the wait kind and its
    /// correlation must match the committed ticket; an ordinary message or a
    /// stale approval can never move the call into `Executing`.
    pub fn resume_executing(
        &mut self,
        call_id: &str,
        expected_kind: ToolWaitKind,
        correlation_id: &str,
    ) -> Result<PendingToolExecutionEntry<'_>, ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let correlation_matches = match &call.phase {
            ToolCallPhase::Awaiting { wait } => wait.correlation_id == correlation_id,
            _ => false,
        };
        let attempt = next_execution_attempt(
            phase_view(&call.phase),
            call.recovery_policy.max_attempts(),
            ExecutionRequest::Resume {
                expected_kind,
                correlation_matches,
            },
        )?;
        Ok(PendingToolExecutionEntry::new(call, attempt))
    }

    pub fn mark_awaiting(
        &mut self,
        call_id: &str,
        kind: ToolWaitKind,
        correlation_id: impl Into<String>,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let another_call_is_awaiting = self.calls.iter().any(|entry| {
            entry.call.call_id != call_id && matches!(entry.phase, ToolCallPhase::Awaiting { .. })
        });
        let call = self.call_mut(call_id)?;
        if !permits_await_transition(phase_view(&call.phase), another_call_is_awaiting) {
            if another_call_is_awaiting {
                return Err(ToolBatchError::MultipleAwaitingCalls);
            }
            return Err(ToolBatchError::InvalidTransition);
        }
        let correlation_id = correlation_id.into();
        if correlation_id.is_empty() {
            return Err(ToolBatchError::EmptyWaitCorrelation);
        }
        call.phase = ToolCallPhase::Awaiting {
            wait: ToolWait {
                kind,
                correlation_id,
            },
        };
        Ok(())
    }

    /// Validate that this aggregate is the exact execution truth named by a
    /// committed ResumeTicket. Every identity and payload axis is checked, so a
    /// stale ticket cannot resume another Run, Thread, call, tool, or wait.
    pub fn validate_awaiting_ticket(
        &self,
        expected_thread: &ThreadId,
        ticket: &ResumeTicket,
    ) -> Result<(), ToolBatchWaitError> {
        if self.phase == ToolBatchPhase::Finalized {
            return Err(ToolBatchWaitError::Finalized);
        }
        if ticket.run_id != self.run_id {
            return Err(ToolBatchWaitError::RunMismatch);
        }
        if &ticket.thread_id != expected_thread {
            return Err(ToolBatchWaitError::ThreadMismatch);
        }
        let AwaitTarget::ToolCall {
            reason,
            call_id,
            tool,
            authority,
        } = ticket.target()
        else {
            return Err(ToolBatchWaitError::NotToolCall);
        };
        if *authority == ToolCallAuthority::RunAttemptExecutor {
            return Err(ToolBatchWaitError::AuthorityMismatch);
        }
        let entry = self
            .calls
            .iter()
            .find(|entry| matches!(entry.phase, ToolCallPhase::Awaiting { .. }))
            .ok_or(ToolBatchWaitError::MissingAwaitingCall)?;
        if entry.call.call_id != *call_id {
            return Err(ToolBatchWaitError::CallMismatch);
        }
        let ToolCallPhase::Awaiting { wait } = &entry.phase else {
            unreachable!("entry selected from awaiting calls")
        };
        if wait.kind != tool_wait_kind(*reason) {
            return Err(ToolBatchWaitError::KindMismatch);
        }
        if wait.correlation_id != ticket.correlation_id {
            return Err(ToolBatchWaitError::CorrelationMismatch);
        }
        if entry.call.tool_id != tool.tool_id || entry.call.arguments != tool.arguments {
            return Err(ToolBatchWaitError::ToolMismatch);
        }
        Ok(())
    }

    /// Checkpoint a physical return without applying its state or running hooks.
    /// Exact replay is inert; no known return can be replaced by another value.
    pub fn record_returned(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(&output.call_id)?;
        if let ToolCallPhase::Returned(previous) = &call.phase {
            return if previous == &output {
                Ok(())
            } else {
                Err(ToolBatchError::InvalidTransition)
            };
        }
        if !permits_call_transition(phase_view(&call.phase), CallTransition::RecordReturn) {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.phase = ToolCallPhase::Returned(output);
        Ok(())
    }

    /// Retain an accepted resume prefix behind the same publication barrier.
    /// Prefix-only staging is legal only for an explicitly known raw result.
    pub fn set_returned_context(
        &mut self,
        call_id: &str,
        messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        if !matches!(call.phase, ToolCallPhase::Returned(_))
            || messages
                .iter()
                .any(|message| message.role != awaken_agent_contract::agent::message::Role::System)
        {
            return Err(ToolBatchError::InvalidTransition);
        }
        if !call.result_messages.is_empty() && call.result_messages != messages {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.result_messages = messages;
        Ok(())
    }

    /// Materialization changes only model-visible content, never the tool's own
    /// state or identity. The exact raw value remains in its prior ThreadCommit.
    pub fn complete_returned(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(&output.call_id)?;
        let ToolCallPhase::Returned(raw) = &call.phase else {
            return Err(ToolBatchError::InvalidTransition);
        };
        if !permits_call_transition(phase_view(&call.phase), CallTransition::MaterializeReturn)
            || raw.state != output.state
            || raw.is_error != output.is_error
        {
            return Err(ToolBatchError::InvalidTransition);
        }
        self.complete_with_transition(output, CallTransition::MaterializeReturn)
    }

    pub fn complete(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.complete_with_transition(output, CallTransition::CompleteFromExecutionOrWait)
    }

    /// Complete a call that the runtime answered without entering an external
    /// executor (schema/open, policy block, or a gate-supplied result).
    pub fn complete_immediate(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.complete_with_transition(output, CallTransition::CompleteImmediate)
    }

    // The public completion roles select their original phase admission; one
    // mutable updater preserves lookup/error order and exact replay behavior.
    fn complete_with_transition(
        &mut self,
        output: ToolOutput,
        transition: CallTransition,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(&output.call_id)?;
        let exact_completed_output =
            matches!(&call.phase, ToolCallPhase::Completed(existing) if existing == &output);
        match completion_disposition(phase_view(&call.phase), transition, exact_completed_output) {
            CompletionDisposition::Apply => call.phase = ToolCallPhase::Completed(output),
            CompletionDisposition::Replay => {}
            CompletionDisposition::Reject => return Err(ToolBatchError::InvalidTransition),
        }
        Ok(())
    }

    pub fn set_result_messages(
        &mut self,
        call_id: &str,
        messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        if !permits_call_transition(phase_view(&call.phase), CallTransition::StageResult) {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.result_messages = messages;
        Ok(())
    }

    pub fn set_result_state(
        &mut self,
        call_id: &str,
        state: Vec<awaken_agent_contract::agent::state::Command>,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        if !permits_call_transition(phase_view(&call.phase), CallTransition::StageResult) {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.result_state = state;
        Ok(())
    }

    pub fn mark_indeterminate(
        &mut self,
        call_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        if !permits_call_transition(phase_view(&call.phase), CallTransition::MarkIndeterminate) {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.phase = ToolCallPhase::Indeterminate {
            reason: reason.into(),
        };
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        if !self.is_complete() {
            return Err(ToolBatchError::Incomplete);
        }
        self.phase = ToolBatchPhase::Finalized;
        Ok(())
    }

    /// Borrow the staged effects that the Runtime may publish to the transcript.
    /// `finalize` first proves that every call is terminal; persisted finalized
    /// batches pass the same invariant through `validate` during decoding.
    pub fn publication(&self) -> Result<ToolBatchPublication<'_>, ToolBatchError> {
        if !permits_publication(self.phase) {
            return Err(ToolBatchError::Unfinalized);
        }
        Ok(ToolBatchPublication { calls: &self.calls })
    }

    /// Seal every unfinished call when its owning Run ends.
    ///
    /// A terminal Run can never be resumed, so retaining a `Requested`,
    /// `Executing`, or `Awaiting` call would leave an impossible recovery
    /// instruction in durable state. Completed results are preserved; known raw
    /// becomes KnownAborted, while unfinished unknown calls become Indeterminate.
    /// The same batch is then finalized without publishing invented results.
    /// Reapplying this operation is an idempotent no-op.
    pub fn seal_on_run_end(&mut self, reason: &str) {
        if self.phase == ToolBatchPhase::Finalized {
            return;
        }
        for call in &mut self.calls {
            if abandon_known_return(call, reason) {
                continue;
            } else if must_seal_on_run_end(phase_view(&call.phase)) {
                call.phase = ToolCallPhase::Indeterminate {
                    reason: reason.to_string(),
                };
            }
        }
        self.phase = ToolBatchPhase::Finalized;
    }

    pub fn output(&self, call_id: &str) -> Option<ToolOutput> {
        self.calls
            .iter()
            .find(|entry| entry.call.call_id == call_id)
            .and_then(|entry| match &entry.phase {
                ToolCallPhase::Completed(output) => Some(output.clone()),
                ToolCallPhase::Indeterminate { reason } => Some(ToolOutput::error(
                    call_id,
                    format!("tool outcome is indeterminate: {reason}"),
                )),
                _ => None,
            })
    }

    fn ensure_open(&self) -> Result<(), ToolBatchError> {
        if self.phase == ToolBatchPhase::Finalized {
            Err(ToolBatchError::Finalized)
        } else {
            Ok(())
        }
    }

    fn call_mut(&mut self, call_id: &str) -> Result<&mut DurableToolCall, ToolBatchError> {
        self.calls
            .iter_mut()
            .find(|entry| entry.call.call_id == call_id)
            .ok_or_else(|| ToolBatchError::UnknownCall(call_id.to_string()))
    }
}

fn abandon_known_return(call: &mut DurableToolCall, reason: &str) -> bool {
    let ToolCallPhase::Returned(output) = &call.phase else {
        return false;
    };
    call.phase = ToolCallPhase::KnownAborted {
        output: output.clone(),
        reason: reason.to_owned(),
    };
    call.result_messages.clear();
    call.result_state.clear();
    true
}

/// The current batch is an explicit Run-scoped entity encoded through the common
/// commit log. A finalized batch remains until the next batch replaces it, which
/// preserves crash evidence without creating a second repository.
pub struct ActiveToolBatch;

impl StateKey for ActiveToolBatch {
    const KEY: &'static str = "runtime.active_tool_batch.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<ToolBatch>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use awaken_agent_contract::agent::awaiting::{PauseReason, PendingTool};

    pub(super) fn batch() -> ToolBatch {
        batch_with_policy(ToolRecoveryPolicy::replay_safe())
    }

    pub(super) fn batch_with_policy(policy: ToolRecoveryPolicy) -> ToolBatch {
        ToolBatch::for_step(
            RunId("r".into()),
            0,
            [(
                ToolCall {
                    call_id: "c".into(),
                    tool_id: "t".into(),
                    arguments: serde_json::json!({}),
                },
                policy,
            )],
        )
        .unwrap()
    }

    fn two_call_batch() -> ToolBatch {
        ToolBatch::for_step(
            RunId("r".into()),
            0,
            ["c1", "c2"].map(|call_id| {
                (
                    ToolCall {
                        call_id: call_id.into(),
                        tool_id: format!("tool-{call_id}"),
                        arguments: serde_json::json!({"call": call_id}),
                    },
                    ToolRecoveryPolicy::replay_safe(),
                )
            }),
        )
        .unwrap()
    }

    fn tool_ticket(
        reason: ToolAwaitReason,
        call_id: &str,
        tool_id: &str,
        correlation_id: &str,
    ) -> ResumeTicket {
        ResumeTicket::new(
            correlation_id,
            RunId("r".into()),
            ThreadId("thread".into()),
            "snapshot",
            "catalog",
            AwaitTarget::ToolCall {
                reason,
                call_id: call_id.into(),
                tool: PendingTool {
                    tool_id: tool_id.into(),
                    arguments: serde_json::json!({"call": call_id}),
                },
                authority: ToolCallAuthority::RuntimeToolBatch,
            },
        )
    }

    #[test]
    fn operation_identity_is_owned_by_the_batch_coordinate() {
        // Cause/effect graph: C1 Run and step select one durable batch; C2 the
        // provider call id is equal or different. Effects: E1 exact recovery
        // reconstructs the same deterministic operation id; E2 another call
        // cannot collide.
        // Decision table: O1(same C1+C2)->E1; O2(same C1,!C2)->E2.
        // Constraints/invariants: Run+Step+call form the sole operation coordinate;
        // reconstruction is deterministic and call ids remain collision-free.
        let batch = ToolBatch::for_step(
            RunId("run-7".into()),
            3,
            [(
                ToolCall {
                    call_id: "call-1".into(),
                    tool_id: "tool".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::replay_safe(),
            )],
        )
        .unwrap();
        let operation_id = batch.operation_id("call-1");
        assert_eq!(operation_id, "tool-batch:run-7:3:call-1", "O1/E1");
        assert_eq!(
            operation_id,
            ToolBatch::operation_id_for_step(&RunId("run-7".into()), 3, "call-1"),
            "O1/E1 deterministic reconstruction from the durable coordinate"
        );
        assert_ne!(operation_id, batch.operation_id("call-2"), "O2/E2");
    }

    #[test]
    fn executor_entry_admission_follows_the_complete_phase_table() {
        // Causes: C1 the exact call is Requested, Executing, Awaiting,
        // Completed, or Indeterminate; C2 the requested call identity is exact
        // or absent. Effects: E1 only Executing yields a borrowed admission
        // carrying its exact attempt and batch-owned operation identity; E2
        // every other known phase rejects; E3 an absent identity rejects.
        // Decision rules: R1=(C1 Executing)+C2 exact -> E1;
        // R2=(C1 other)+C2 exact -> E2; R3=!C2 -> E3. The aggregate remains the
        // sole phase and identity owner in all rules.
        let mut awaiting = batch();
        assert!(matches!(
            awaiting.execution_admission("c"),
            Err(ToolBatchError::InvalidTransition)
        ));
        assert!(matches!(
            awaiting.execution_admission("missing"),
            Err(ToolBatchError::UnknownCall(call_id)) if call_id == "missing"
        ));

        assert_eq!(awaiting.mark_executing("c"), Ok(1));
        let admitted = awaiting.execution_admission("c").expect("R1/E1");
        assert_eq!(admitted.call_id(), "c", "R1/E1 exact call");
        assert_eq!(admitted.attempt(), 1, "R1/E1 exact attempt");
        assert_eq!(
            admitted.operation_id(),
            "tool-batch:r:0:c",
            "R1/E1 canonical operation identity"
        );

        awaiting
            .mark_awaiting("c", ToolWaitKind::ExternalResult, "wait")
            .expect("build Awaiting row");
        assert!(matches!(
            awaiting.execution_admission("c"),
            Err(ToolBatchError::InvalidTransition)
        ));
        awaiting
            .complete(ToolOutput::ok("c", "done"))
            .expect("build Completed row");
        assert!(matches!(
            awaiting.execution_admission("c"),
            Err(ToolBatchError::InvalidTransition)
        ));

        let mut indeterminate = batch();
        indeterminate
            .mark_indeterminate("c", "unknown")
            .expect("build Indeterminate row");
        assert!(matches!(
            indeterminate.execution_admission("c"),
            Err(ToolBatchError::InvalidTransition)
        ));
    }

    #[test]
    fn terminal_output_is_idempotent_and_finalization_requires_completion() {
        // Updater consolidation: both public roles retain one exact-output
        // comparison and mutation path. The immediate exact/changed rows below
        // complement the original executor replay assertions; raw output and
        // independently staged state remain distinct replay coordinates.
        // Causes: C1 completion source is executor/wait or immediate; C2 the
        // predecessor is its one admissible nonterminal phase or Completed;
        // C3 a Completed replay has the exact or changed output. Effects: E1 an
        // admissible nonterminal result applies once; E2 exact Completed replay
        // is a no-op; E3 changed Completed replay rejects without mutation.
        // Rules C1+C2=>E1, Completed+C3(exact)=>E2, Completed+!C3=>E3.
        // Raw/staged compatibility table: R1 raw=[A], staged=[A,B]; R2
        // raw=[A,B], staged=[A,B]. Both are legal completed outcomes, but
        // only the exact RAW output replays. Equal staged state cannot make
        // R1 and R2 interchangeable. No hook runs to reconstruct either row.
        let raw = StateCommand::set(Scope::Thread, MergePolicy::Disjoint, "raw", 1.into());
        let reaction =
            StateCommand::set(Scope::Thread, MergePolicy::Disjoint, "reaction", 2.into());
        let combined = vec![raw.clone(), reaction];
        let raw_output = ToolOutput::ok("c", "done").with_state(vec![raw]);
        let combined_output = ToolOutput::ok("c", "done").with_state(combined.clone());
        let mut raw_then_reaction = batch();
        let mut raw_only = batch();
        for (candidate, output) in [
            (&mut raw_then_reaction, raw_output.clone()),
            (&mut raw_only, combined_output.clone()),
        ] {
            candidate.mark_executing("c").unwrap();
            candidate.complete(output).unwrap();
            candidate.set_result_state("c", combined.clone()).unwrap();
            candidate.validate().unwrap();
        }
        assert_eq!(
            raw_then_reaction.calls()[0].result_state,
            raw_only.calls()[0].result_state
        );
        let original_raw = raw_then_reaction.clone();
        let original_combined = raw_only.clone();
        raw_then_reaction.complete(raw_output.clone()).unwrap();
        assert_eq!(raw_then_reaction, original_raw, "R1 exact raw replay");
        raw_then_reaction
            .complete_immediate(raw_output.clone())
            .unwrap();
        assert_eq!(
            raw_then_reaction, original_raw,
            "R1 immediate exact raw replay is inert"
        );
        assert_eq!(
            raw_then_reaction.complete_immediate(combined_output.clone()),
            Err(ToolBatchError::InvalidTransition),
            "R1 immediate replay cannot substitute equal staged state for exact raw output"
        );
        assert_eq!(
            raw_then_reaction, original_raw,
            "R1 immediate rejection preserves all staged effects"
        );
        assert_eq!(
            raw_only.complete(raw_output),
            Err(ToolBatchError::InvalidTransition),
            "R2 cannot derive raw output from its equal staged vector"
        );
        assert_eq!(raw_only, original_combined, "R2 rejection is inert");
        assert_eq!(
            raw_then_reaction.complete(combined_output.clone()),
            Err(ToolBatchError::InvalidTransition),
            "R1 staged equality does not change raw replay identity"
        );
        assert_eq!(raw_then_reaction, original_raw, "R1 rejection is inert");
        raw_only.complete(combined_output).unwrap();
        assert_eq!(raw_only, original_combined, "R2 exact raw replay");

        let mut batch = batch();
        assert_eq!(batch.finalize(), Err(ToolBatchError::Incomplete));
        batch.mark_executing("c").unwrap();
        let output = ToolOutput::ok("c", "done");
        batch.complete(output.clone()).unwrap();
        let completed = batch.clone();
        batch.complete(output).unwrap();
        assert_eq!(batch, completed, "E2 exact replay is inert");
        assert_eq!(
            batch.complete(ToolOutput::ok("c", "changed")),
            Err(ToolBatchError::InvalidTransition),
            "E3 changed replay is rejected"
        );
        assert_eq!(batch, completed, "E3 rejection preserves the aggregate");
        batch.finalize().unwrap();
        assert_eq!(batch.phase, ToolBatchPhase::Finalized);
        assert_eq!(batch.mark_executing("c"), Err(ToolBatchError::Finalized));
    }

    #[test]
    fn known_return_checkpoints_preserve_raw_and_never_reenter_physical_execution() {
        // J1/J6/J10 cause/effect table: Requested/Executing/Awaiting -> exact
        // Returned; identical replay stutters, changed raw rejects unchanged;
        // Returned has no executor/recovery admission. Materialization may
        // change content only; raw state/error identity must remain exact.
        for phase in [
            ToolCallPhase::Requested,
            ToolCallPhase::Executing { attempt: 1 },
            ToolCallPhase::Awaiting {
                wait: ToolWait {
                    kind: ToolWaitKind::ExternalResult,
                    correlation_id: "reply".into(),
                },
            },
        ] {
            let mut candidate = batch();
            candidate.calls[0].phase = phase;
            let raw = ToolOutput::ok("c", "physical raw").with_state(vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "own",
                7.into(),
            )]);
            candidate.record_returned(raw.clone()).unwrap();
            let known = candidate.clone();
            candidate.record_returned(raw.clone()).unwrap();
            assert_eq!(candidate, known, "exact return replay stutters");
            assert!(
                candidate
                    .record_returned(ToolOutput::ok("c", "changed"))
                    .is_err()
            );
            assert_eq!(candidate, known, "changed raw cannot overwrite evidence");
            assert!(candidate.execution_admission("c").is_err());
            assert!(
                candidate
                    .recover_execution("c", ToolRecoveryCapability::ReplaySafe)
                    .is_err()
            );
            assert_eq!(
                candidate, known,
                "known return never obtains physical entry"
            );
            let encoded = serde_json::to_value(&candidate).unwrap();
            let decoded: ToolBatch = serde_json::from_value(encoded).unwrap();
            assert_eq!(
                decoded, known,
                "raw content/error/state survive the owner codec"
            );
            let mut materialized = raw.clone();
            materialized.content = vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "preview",
            )];
            let mut invalid = materialized.clone();
            invalid.state.clear();
            assert!(candidate.complete_returned(invalid).is_err());
            assert_eq!(
                candidate, known,
                "materialization cannot erase tool-owned state"
            );
            candidate.complete_returned(materialized.clone()).unwrap();
            assert_eq!(candidate.output("c"), Some(materialized));
            assert!(
                candidate.calls()[0].result_state.is_empty(),
                "raw does not invent publication"
            );
        }
    }

    #[test]
    fn terminal_sealing_distinguishes_known_aborted_from_unknown_and_old_readers_reject_it() {
        // J6/J9: a known raw + Run terminal => KnownAborted with exact payload,
        // no model result or staged command. An unknown entered effect retains
        // the existing Indeterminate meaning. Both are absorbing; old enum
        // readers reject new variants rather than guessing legacy Completed.
        #[derive(serde::Deserialize)]
        enum OldPhase {
            Requested,
            Executing { attempt: u16 },
            Awaiting { wait: ToolWait },
            Completed(ToolOutput),
            Indeterminate { reason: String },
        }
        for value in [
            serde_json::json!("Requested"),
            serde_json::json!({"Executing":{"attempt":1}}),
            serde_json::json!({"Awaiting":{"wait":{"kind":"ExternalResult","correlation_id":"reply"}}}),
            serde_json::json!({"Completed":ToolOutput::ok("c", "legacy")}),
            serde_json::json!({"Indeterminate":{"reason":"unknown"}}),
        ] {
            match serde_json::from_value::<OldPhase>(value).unwrap() {
                OldPhase::Requested => {}
                OldPhase::Executing { attempt } => assert_eq!(attempt, 1),
                OldPhase::Awaiting { wait } => assert_eq!(wait.correlation_id, "reply"),
                OldPhase::Completed(output) => assert_eq!(output.call_id, "c"),
                OldPhase::Indeterminate { reason } => assert_eq!(reason, "unknown"),
            }
        }
        let mut candidate = batch();
        let raw = ToolOutput::ok("c", "known");
        candidate.record_returned(raw.clone()).unwrap();
        assert!(
            serde_json::from_value::<OldPhase>(
                serde_json::to_value(&candidate.calls[0].phase).unwrap()
            )
            .is_err()
        );
        candidate.seal_on_run_end("cancelled");
        assert!(matches!(&candidate.calls[0].phase,
            ToolCallPhase::KnownAborted { output, .. } if output == &raw));
        assert!(candidate.output("c").is_none(), "no fabricated ToolResult");
        assert!(candidate.publication().unwrap().messages().next().is_none());
        let sealed = candidate.clone();
        candidate.seal_on_run_end("again");
        assert_eq!(candidate, sealed);
        assert!(
            serde_json::from_value::<OldPhase>(
                serde_json::to_value(&candidate.calls[0].phase).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn publication_requires_finalization_and_preserves_model_order_exactly_once() {
        // Causes: C1 batch is Open/Finalized; C2 calls are incomplete/terminal;
        // C3 two calls complete in model/reverse order. Effects: E1 only a
        // Finalized batch exposes a publication view; E2 messages and state are
        // each yielded once in original model order. Constraints: finalization
        // itself requires every call terminal, while terminal-but-Open remains
        // private. Decision rules: P1(Open,incomplete,*) -> reject; P2(Open,
        // terminal,*) -> reject; P3(Finalized,terminal,model/reverse) -> E1/E2.
        // P4 raw state empty + independently staged hook state is legal; a
        // codec round-trip preserves those distinct values and still refuses
        // publication while Open. Deriving staged effects from raw output
        // would lose P4; exposing them before Finalized would bypass P2.
        let mut batch = two_call_batch();
        assert!(matches!(
            batch.publication(),
            Err(ToolBatchError::Unfinalized)
        ));
        for call_id in ["c2", "c1"] {
            batch.mark_executing(call_id).unwrap();
            batch
                .complete(ToolOutput::ok(call_id, format!("output-{call_id}")))
                .unwrap();
            batch
                .set_result_messages(
                    call_id,
                    vec![awaken_agent_contract::agent::message::Message::new(
                        awaken_agent_contract::agent::message::Id(format!("message-{call_id}")),
                        awaken_agent_contract::agent::message::Role::Tool,
                        Vec::new(),
                    )],
                )
                .unwrap();
            batch
                .set_result_state(
                    call_id,
                    vec![StateCommand::set(
                        Scope::Run,
                        MergePolicy::Disjoint,
                        format!("state-{call_id}"),
                        serde_json::json!(call_id),
                    )],
                )
                .unwrap();
        }
        assert!(matches!(
            batch.publication(),
            Err(ToolBatchError::Unfinalized)
        ));
        for entry in batch.calls() {
            let ToolCallPhase::Completed(output) = &entry.phase else {
                panic!("P4 calls are terminal")
            };
            assert!(output.state.is_empty(), "P4 raw output owns no state");
            assert_eq!(
                entry.result_state.len(),
                1,
                "P4 staged hook state is retained"
            );
        }
        let decoded: ToolBatch =
            serde_json::from_value(serde_json::to_value(&batch).unwrap()).unwrap();
        assert_eq!(decoded, batch, "P4 the original codec preserves both roles");
        assert!(
            matches!(decoded.publication(), Err(ToolBatchError::Unfinalized)),
            "P4 no Open publication grant"
        );
        batch.finalize().unwrap();
        let publication = batch.publication().unwrap();
        assert_eq!(
            publication
                .messages()
                .map(|message| message.id.0.as_str())
                .collect::<Vec<_>>(),
            ["message-c1", "message-c2"]
        );
        assert_eq!(
            publication
                .result_state()
                .map(|command| command.key.0.as_str())
                .collect::<Vec<_>>(),
            ["state-c1", "state-c2"]
        );
    }

    #[test]
    fn aggregate_result_commands_follow_the_complete_phase_decision_table() {
        // Causes: C1 predecessor phase (seven closed variants), C2 command,
        // C3 exact/missing call identity, C4 open/finalized batch.
        // Effects: E1 accepted commands produce the named terminal phase;
        // E2 every rejection preserves the entire aggregate. Constraints:
        // Completed replay uses the identical output; only complete batches
        // can be finalized. This table is independent of permits_call_transition
        // and exercises the aggregate callers too, so bypassing a guard cannot
        // hide behind a correct Kani kernel.
        //
        // Rule | Phase         | complete | immediate | indeterminate | stage
        // R1   | Requested     | reject   | Completed | Indeterminate | reject
        // R2   | Executing     | Completed| reject    | Indeterminate | reject
        // R3   | Awaiting      | Completed| reject    | Indeterminate | reject
        // R4   | Completed     | replay   | replay    | reject        | accept
        // R5   | Indeterminate | reject   | reject    | Indeterminate | accept
        // R8   | Returned      | reject   | reject    | reject        | reject
        // R9   | KnownAborted  | reject   | reject    | reject        | reject
        // R6: !C3 overrides R1-R5 with E2; R7: !C4 overrides with E2.
        // Shared-updater cause/effect refinement: R7 returns Finalized even
        // for an unknown call; otherwise R6 returns UnknownCall before phase
        // rejection. Accepted completion changes only the selected call phase;
        // exact Completed replay and every error preserve the whole aggregate.
        // The existing replay test adds changed-output coverage for both roles,
        // so another parallel completion table is unnecessary.
        let done = ToolOutput::ok("c", "done");
        for (phase, allowed) in [
            (ToolCallPhase::Requested, [false, true, true, false]),
            (
                ToolCallPhase::Returned(done.clone()),
                [false, false, false, false],
            ),
            (
                ToolCallPhase::KnownAborted {
                    output: done.clone(),
                    reason: "ended".into(),
                },
                [false, false, false, false],
            ),
            (
                ToolCallPhase::Executing { attempt: 1 },
                [true, false, true, false],
            ),
            (
                ToolCallPhase::Awaiting {
                    wait: ToolWait {
                        kind: ToolWaitKind::ToolPermission,
                        correlation_id: "ticket".into(),
                    },
                },
                [true, false, true, false],
            ),
            (
                ToolCallPhase::Completed(done.clone()),
                [true, true, false, true],
            ),
            (
                ToolCallPhase::Indeterminate {
                    reason: "prior".into(),
                },
                [false, false, true, true],
            ),
        ] {
            for (command, accepts_phase) in allowed.into_iter().enumerate() {
                for exact in [true, false] {
                    for finalized in [false, true] {
                        if finalized && !phase.is_terminal() {
                            continue;
                        }
                        let mut candidate = batch();
                        candidate.calls[0].phase = phase.clone();
                        if finalized {
                            candidate.phase = ToolBatchPhase::Finalized;
                        }
                        candidate.validate().unwrap();
                        let before = candidate.clone();
                        let id = if exact { "c" } else { "missing" };
                        let result = match command {
                            0 => candidate.complete(ToolOutput::ok(id, "done")),
                            1 => candidate.complete_immediate(ToolOutput::ok(id, "done")),
                            2 => candidate.mark_indeterminate(id, "unknown"),
                            3 => candidate.set_result_messages(id, Vec::new()),
                            _ => unreachable!(),
                        };
                        let accepted = accepts_phase && exact && !finalized;
                        assert_eq!(
                            result.is_ok(),
                            accepted,
                            "{phase:?}/{command}/{exact}/{finalized}"
                        );
                        if !accepted {
                            assert_eq!(candidate, before, "E2 rejected command stutters");
                            let expected = if finalized {
                                ToolBatchError::Finalized
                            } else if !exact {
                                ToolBatchError::UnknownCall(id.into())
                            } else {
                                ToolBatchError::InvalidTransition
                            };
                            assert_eq!(result, Err(expected), "R6/R7 exact error precedence");
                        } else {
                            match command {
                                0 | 1 => {
                                    assert_eq!(
                                        candidate.calls[0].phase,
                                        ToolCallPhase::Completed(done.clone())
                                    );
                                    let mut expected = before;
                                    expected.calls[0].phase =
                                        ToolCallPhase::Completed(done.clone());
                                    assert_eq!(
                                        candidate, expected,
                                        "E1 completion preserves all other aggregate fields"
                                    );
                                }
                                2 => assert_eq!(
                                    candidate.calls[0].phase,
                                    ToolCallPhase::Indeterminate {
                                        reason: "unknown".into()
                                    }
                                ),
                                3 => assert_eq!(candidate.calls[0].phase, phase),
                                _ => unreachable!(),
                            }
                            candidate.validate().unwrap();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn attempt_budget_is_enforced() {
        let mut batch = batch();
        assert_eq!(batch.mark_executing("c"), Ok(1));
        assert_eq!(batch.mark_executing("c"), Ok(2));
        assert_eq!(batch.mark_executing("c"), Ok(3));
        assert_eq!(
            batch.mark_executing("c"),
            Err(ToolBatchError::AttemptsExhausted)
        );
    }

    fn recover_and_enter(
        batch: &mut ToolBatch,
        capability: ToolRecoveryCapability,
    ) -> Result<Option<u16>, ToolBatchError> {
        match batch.recover_execution("c", capability)? {
            ToolRecoveryAction::Entered { entry } => entry.enter(None).map(Some),
            ToolRecoveryAction::Indeterminate => Ok(None),
        }
    }

    #[test]
    fn recovery_policy_and_attempt_budget_have_one_phase_owner() {
        // Causes: C1 durable phase is Requested, Executing, Awaiting, Completed,
        // or Indeterminate; C2 pinned mode is NeverReplay, ReplaySafe,
        // Idempotent, or DurableRequest; C3 trusted capability is any matching
        // or nonmatching capability; C4 an Executing attempt has or lacks budget.
        // Effects: E1 Requested+supported enters attempt 1 (NeverReplay is safe
        // because no executor was entered); E2 Executing+supported replay mode
        // with budget enters exactly n+1; E3 Executing+NeverReplay, unsupported
        // policy, or exhausted budget becomes Indeterminate; E4 every other
        // phase is rejected without mutation. Decision rules R1=C1(Requested)+
        // C3(supported)=>E1, R2=C1(Executing)+C2(replay)+C3(supported)+C4(room)
        // =>E2, R3=Executing+NeverReplay=>E3, R4=!C3=>E3,
        // R5=Executing+replay+C3+!C4=>E3, R6=C1(other)=>E4.
        // The Runtime persists this aggregate decision before invoking; async
        // commit, crash timing, and external idempotency stay outside this table.
        let modes = [
            ToolRecoveryMode::NeverReplay,
            ToolRecoveryMode::ReplaySafe,
            ToolRecoveryMode::Idempotent,
            ToolRecoveryMode::DurableRequest,
        ];
        let capabilities = [
            ToolRecoveryCapability::NonRecoverable,
            ToolRecoveryCapability::ReplaySafe,
            ToolRecoveryCapability::Idempotent,
            ToolRecoveryCapability::DurableRequest,
        ];
        let candidate = |mode, max_attempts, phase| {
            let mut batch = ToolBatch::for_step(
                RunId("r".into()),
                0,
                [(
                    ToolCall {
                        call_id: "c".into(),
                        tool_id: "t".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolRecoveryPolicy::try_new(mode, max_attempts).unwrap(),
                )],
            )
            .unwrap();
            batch.calls[0].phase = phase;
            batch
        };

        for mode in modes {
            for capability in capabilities {
                let supported = crate::tool::recovery_capability_expected(mode, capability);
                let mut requested = candidate(mode, 3, ToolCallPhase::Requested);
                assert_eq!(
                    recover_and_enter(&mut requested, capability),
                    Ok(if supported { Some(1) } else { None }),
                    "R1/R4 requested {mode:?}/{capability:?}"
                );
                assert_eq!(
                    matches!(
                        requested.calls[0].phase,
                        ToolCallPhase::Executing { attempt: 1 }
                    ),
                    supported,
                    "R1 exact attempt"
                );
                if !supported {
                    assert!(
                        matches!(
                            requested.calls[0].phase,
                            ToolCallPhase::Indeterminate { .. }
                        ),
                        "R4 unsupported Requested is terminal"
                    );
                }

                let mut executing = candidate(mode, 3, ToolCallPhase::Executing { attempt: 1 });
                let replays = mode != ToolRecoveryMode::NeverReplay && supported;
                assert_eq!(
                    recover_and_enter(&mut executing, capability),
                    Ok(if replays { Some(2) } else { None }),
                    "R2/R3/R4 executing {mode:?}/{capability:?}"
                );
                assert_eq!(
                    matches!(
                        executing.calls[0].phase,
                        ToolCallPhase::Executing { attempt: 2 }
                    ),
                    replays,
                    "R2 exact next attempt"
                );
                if !replays {
                    assert!(
                        matches!(
                            executing.calls[0].phase,
                            ToolCallPhase::Indeterminate { .. }
                        ),
                        "R3/R4 rejected reentry is terminal"
                    );
                }
            }
        }

        let mut exhausted = candidate(
            ToolRecoveryMode::ReplaySafe,
            1,
            ToolCallPhase::Executing { attempt: 1 },
        );
        assert_eq!(
            recover_and_enter(&mut exhausted, ToolRecoveryCapability::ReplaySafe),
            Ok(None),
            "R5"
        );
        assert!(matches!(
            &exhausted.calls[0].phase,
            ToolCallPhase::Indeterminate { reason }
                if reason == "recovery attempt budget exhausted"
        ));

        for phase in [
            ToolCallPhase::Awaiting {
                wait: ToolWait {
                    kind: ToolWaitKind::ExternalResult,
                    correlation_id: "corr".into(),
                },
            },
            ToolCallPhase::Completed(ToolOutput::ok("c", "done")),
            ToolCallPhase::Indeterminate {
                reason: "already terminal".into(),
            },
        ] {
            let mut invalid = candidate(ToolRecoveryMode::ReplaySafe, 3, phase);
            let before = invalid.clone();
            assert_eq!(
                recover_and_enter(&mut invalid, ToolRecoveryCapability::ReplaySafe),
                Err(ToolBatchError::InvalidTransition),
                "R6"
            );
            assert_eq!(invalid, before, "R6 rejected recovery stutters");
        }
    }

    #[test]
    fn approval_must_match_the_committed_wait() {
        let mut batch = batch();
        batch
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "approval-1")
            .unwrap();
        assert_eq!(
            batch
                .resume_executing("c", ToolWaitKind::ToolPermission, "stale")
                .err(),
            Some(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch
                .resume_executing("c", ToolWaitKind::Delegation, "approval-1")
                .err(),
            Some(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch
                .resume_executing("c", ToolWaitKind::ToolPermission, "approval-1")
                .and_then(|entry| entry.enter(None)),
            Ok(1)
        );
    }

    #[test]
    fn one_batch_admits_only_one_nonempty_durable_wait() {
        // Cause/effect graph: C1 target call is in an awaitable phase; C2 another
        // call already awaits; C3 correlation is empty. Effects: E1 establish
        // the sole wait; E2 reject a parallel wait; E3 reject an unrecoverable
        // correlation. Decision rules: W1=C1+!C2+!C3=>E1,
        // W2=C1+C2=>E2, W3=C1+!C2+C3=>E3.
        // Constraint: a Run owns at most one ResumeTicket, therefore its active
        // batch must own at most one Awaiting call.
        let mut batch = two_call_batch();
        batch
            .mark_awaiting("c1", ToolWaitKind::ToolPermission, "corr-1")
            .unwrap();
        assert_eq!(
            batch.mark_awaiting("c2", ToolWaitKind::ExternalResult, "corr-2"),
            Err(ToolBatchError::MultipleAwaitingCalls),
            "W2/E2"
        );

        let mut empty = two_call_batch();
        assert_eq!(
            empty.mark_awaiting("c1", ToolWaitKind::ToolPermission, ""),
            Err(ToolBatchError::EmptyWaitCorrelation),
            "W3/E3"
        );
    }

    #[test]
    fn resume_ticket_must_match_every_durable_wait_axis() {
        // Cause/effect graph: C1 batch is open; C2 exactly one call awaits;
        // C3 Run matches; C4 Thread matches; C5 target is a tool call; C6 call
        // id matches; C7 reason maps to the wait kind; C8 correlation matches;
        // C9 tool id and arguments match. Effect E1 proves exact coherence by
        // returning unit without exposing or mutating the DurableToolCall; each
        // negated cause fails closed with its specific error and no state
        // transition. The decision table has four positive mapping rules
        // A1a..A1d=C1..C9=>E1, one for every durable wait reason, and one MC/DC
        // rule A2..A10 for each independently negated cause.
        for (rule, reason, kind) in [
            (
                "A1a",
                ToolAwaitReason::Permission,
                ToolWaitKind::ToolPermission,
            ),
            (
                "A1b",
                ToolAwaitReason::ClientExecution,
                ToolWaitKind::ExternalResult,
            ),
            (
                "A1c",
                ToolAwaitReason::ScheduledAction,
                ToolWaitKind::ScheduledAction,
            ),
            ("A1d", ToolAwaitReason::Delegation, ToolWaitKind::Delegation),
        ] {
            let mut mapped = two_call_batch();
            mapped.mark_awaiting("c1", kind, "corr").unwrap();
            let ticket = tool_ticket(reason, "c1", "tool-c1", "corr");
            let before = mapped.clone();
            assert_eq!(
                mapped.validate_awaiting_ticket(&ThreadId("thread".into()), &ticket),
                Ok(()),
                "{rule}/E1"
            );
            assert_eq!(mapped, before, "{rule}/E1 is validation-only");
        }

        let mut batch = two_call_batch();
        batch
            .mark_awaiting("c1", ToolWaitKind::ToolPermission, "corr")
            .unwrap();
        let exact = tool_ticket(ToolAwaitReason::Permission, "c1", "tool-c1", "corr");

        let mut wrong_run = exact.clone();
        wrong_run.run_id = RunId("other".into());
        let mut wrong_thread = exact.clone();
        wrong_thread.thread_id = ThreadId("other".into());
        let not_tool = ResumeTicket::new(
            "corr",
            RunId("r".into()),
            ThreadId("thread".into()),
            "snapshot",
            "catalog",
            AwaitTarget::Pause(PauseReason::Manual),
        );
        let wrong_call = tool_ticket(ToolAwaitReason::Permission, "c2", "tool-c2", "corr");
        let wrong_kind = tool_ticket(ToolAwaitReason::ClientExecution, "c1", "tool-c1", "corr");
        let wrong_correlation = tool_ticket(ToolAwaitReason::Permission, "c1", "tool-c1", "other");
        let wrong_tool = tool_ticket(ToolAwaitReason::Permission, "c1", "other", "corr");

        for (rule, ticket, expected) in [
            ("A2", wrong_run, ToolBatchWaitError::RunMismatch),
            ("A3", wrong_thread, ToolBatchWaitError::ThreadMismatch),
            ("A4", not_tool, ToolBatchWaitError::NotToolCall),
            ("A5", wrong_call, ToolBatchWaitError::CallMismatch),
            ("A6", wrong_kind, ToolBatchWaitError::KindMismatch),
            (
                "A7",
                wrong_correlation,
                ToolBatchWaitError::CorrelationMismatch,
            ),
            ("A8", wrong_tool, ToolBatchWaitError::ToolMismatch),
        ] {
            assert_eq!(
                batch.validate_awaiting_ticket(&ThreadId("thread".into()), &ticket),
                Err(expected),
                "{rule} fails closed"
            );
        }

        let no_wait = two_call_batch();
        assert_eq!(
            no_wait.validate_awaiting_ticket(&ThreadId("thread".into()), &exact),
            Err(ToolBatchWaitError::MissingAwaitingCall),
            "A9"
        );
        let mut finalized = two_call_batch();
        for call_id in ["c1", "c2"] {
            finalized
                .complete_immediate(ToolOutput::ok(call_id, "done"))
                .unwrap();
        }
        finalized.finalize().unwrap();
        assert_eq!(
            finalized.validate_awaiting_ticket(&ThreadId("thread".into()), &exact),
            Err(ToolBatchWaitError::Finalized),
            "A10"
        );
    }

    #[test]
    fn persisted_batch_with_multiple_waits_is_rejected_before_recovery() {
        // Cause/effect graph: C1 a persisted batch contains one or two Awaiting
        // calls. Effects: E1 one is decodable; E2 two are rejected at the typed
        // state boundary. Decision rules P1=C1(one)=>E1, P2=C1(two)=>E2.
        // This mutation represents corrupted/legacy bytes that safe aggregate
        // methods can no longer construct.
        let mut one = two_call_batch();
        one.mark_awaiting("c1", ToolWaitKind::ToolPermission, "corr-1")
            .unwrap();
        let mut value = serde_json::to_value(one).unwrap();
        value["calls"][1]["phase"] = serde_json::json!({
            "Awaiting": {
                "wait": {"kind": "ExternalResult", "correlation_id": "corr-2"}
            }
        });
        assert!(serde_json::from_value::<ToolBatch>(value).is_err(), "P2/E2");
    }

    #[test]
    fn ending_a_run_seals_every_unfinished_call_idempotently() {
        let mut batch = batch();
        batch.mark_executing("c").unwrap();
        batch.seal_on_run_end("run ended");

        assert_eq!(batch.phase, ToolBatchPhase::Finalized);
        assert!(matches!(
            batch.calls[0].phase,
            ToolCallPhase::Indeterminate { ref reason } if reason == "run ended"
        ));

        let sealed = batch.clone();
        batch.seal_on_run_end("different retry reason");
        assert_eq!(batch, sealed);
    }

    #[test]
    fn persisted_finalized_batch_with_requested_work_is_rejected_at_decode() {
        let mut value = serde_json::to_value(batch()).unwrap();
        value["phase"] = serde_json::json!("Finalized");
        assert!(serde_json::from_value::<ToolBatch>(value).is_err());
    }

    #[test]
    fn persisted_empty_or_duplicate_batches_never_construct_the_aggregate() {
        let mut empty = serde_json::to_value(batch()).unwrap();
        empty["calls"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ToolBatch>(empty).is_err());

        let mut duplicate = serde_json::to_value(batch()).unwrap();
        duplicate["calls"] =
            serde_json::json!([duplicate["calls"][0].clone(), duplicate["calls"][0].clone()]);
        assert!(serde_json::from_value::<ToolBatch>(duplicate).is_err());
    }

    #[test]
    fn batch_identity_is_derived_from_and_bound_to_a_non_empty_run() {
        let call = || {
            [(
                ToolCall {
                    call_id: "c".into(),
                    tool_id: "t".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::default(),
            )]
        };
        assert_eq!(
            ToolBatch::for_step(RunId(String::new()), 0, call()),
            Err(ToolBatchError::EmptyRunId)
        );

        let mut mismatched = serde_json::to_value(batch()).unwrap();
        mismatched["id"] = serde_json::json!("tool-batch:another-run:0");
        assert!(serde_json::from_value::<ToolBatch>(mismatched).is_err());
    }

    #[test]
    fn persisted_completed_call_cannot_contain_another_calls_output() {
        // Decoder compatibility table: L1/L2 exact call + nonempty raw state
        // + result_state omitted/explicitly [] => valid, raw state retained,
        // but no staged publication is inferred. Open still refuses publication;
        // Finalized yields zero commands. L3 foreign output call_id => reject.
        // Missing and explicit-empty historical values must not become an
        // implicit raw-state fallback or a new unpublished Task authority.
        let mut batch = batch();
        batch.mark_executing("c").unwrap();
        let raw = StateCommand::set(Scope::Thread, MergePolicy::Disjoint, "raw", 1.into());
        let output = ToolOutput::ok("c", "ok").with_state(vec![raw]);
        batch.complete(output.clone()).unwrap();
        let wire = serde_json::to_value(&batch).unwrap();
        for (rule, omitted) in [("L1", true), ("L2", false)] {
            let mut value = wire.clone();
            let call = value["calls"][0].as_object_mut().unwrap();
            if omitted {
                call.remove("result_state");
            } else {
                call.insert("result_state".into(), serde_json::json!([]));
            }
            let mut decoded: ToolBatch = serde_json::from_value(value).unwrap();
            assert_eq!(
                decoded.output("c"),
                Some(output.clone()),
                "{rule} raw identity retained"
            );
            assert!(
                decoded.calls()[0].result_state.is_empty(),
                "{rule} no inferred staged state"
            );
            assert!(
                matches!(decoded.publication(), Err(ToolBatchError::Unfinalized)),
                "{rule} Open remains private"
            );
            decoded.finalize().unwrap();
            assert!(
                decoded
                    .publication()
                    .unwrap()
                    .result_state()
                    .next()
                    .is_none(),
                "{rule} Finalized does not invent raw publication"
            );
        }
        let mut value = serde_json::to_value(batch).unwrap();
        value["calls"][0]["phase"]["Completed"]["call_id"] = serde_json::json!("different-call");
        assert!(serde_json::from_value::<ToolBatch>(value).is_err());
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const fn listed_wait_kind(kind: ToolWaitKind) -> usize {
        match kind {
            ToolWaitKind::ToolPermission => 0,
            ToolWaitKind::Delegation => 1,
            ToolWaitKind::ScheduledAction => 2,
            ToolWaitKind::ExternalResult => 3,
        }
    }
    const EVERY_WAIT_KIND: [ToolWaitKind; 4] = [
        ToolWaitKind::ToolPermission,
        ToolWaitKind::Delegation,
        ToolWaitKind::ScheduledAction,
        ToolWaitKind::ExternalResult,
    ];

    /// Position of a phase in [`every_phase`]: the five unit phases, then
    /// every `u16` attempt, then every wait kind.
    const fn listed_phase(phase: CallPhaseView) -> usize {
        match phase {
            CallPhaseView::Requested => 0,
            CallPhaseView::Returned => 1,
            CallPhaseView::KnownAborted => 2,
            CallPhaseView::Completed => 3,
            CallPhaseView::Indeterminate => 4,
            CallPhaseView::Executing(attempt) => 5 + attempt as usize,
            CallPhaseView::Awaiting(kind) => 5 + (u16::MAX as usize + 1) + listed_wait_kind(kind),
        }
    }
    /// 5 unit phases + Executing(u16: 65536) + Awaiting(ToolWaitKind: 4).
    const EVERY_PHASE_COUNT: usize = 65_545;

    fn every_phase() -> impl Iterator<Item = CallPhaseView> {
        [
            CallPhaseView::Requested,
            CallPhaseView::Returned,
            CallPhaseView::KnownAborted,
            CallPhaseView::Completed,
            CallPhaseView::Indeterminate,
        ]
        .into_iter()
        .chain((u16::MIN..=u16::MAX).map(CallPhaseView::Executing))
        .chain(EVERY_WAIT_KIND.into_iter().map(CallPhaseView::Awaiting))
    }

    const fn listed_transition(transition: CallTransition) -> usize {
        match transition {
            CallTransition::CompleteFromExecutionOrWait => 0,
            CallTransition::CompleteImmediate => 1,
            CallTransition::RecordReturn => 2,
            CallTransition::MaterializeReturn => 3,
            CallTransition::MarkIndeterminate => 4,
            CallTransition::StageResult => 5,
        }
    }
    const EVERY_TRANSITION: [CallTransition; 6] = [
        CallTransition::CompleteFromExecutionOrWait,
        CallTransition::CompleteImmediate,
        CallTransition::RecordReturn,
        CallTransition::MaterializeReturn,
        CallTransition::MarkIndeterminate,
        CallTransition::StageResult,
    ];

    const fn listed_mode(mode: ToolRecoveryMode) -> usize {
        match mode {
            ToolRecoveryMode::NeverReplay => 0,
            ToolRecoveryMode::ReplaySafe => 1,
            ToolRecoveryMode::Idempotent => 2,
            ToolRecoveryMode::DurableRequest => 3,
        }
    }
    const EVERY_MODE: [ToolRecoveryMode; 4] = [
        ToolRecoveryMode::NeverReplay,
        ToolRecoveryMode::ReplaySafe,
        ToolRecoveryMode::Idempotent,
        ToolRecoveryMode::DurableRequest,
    ];

    const fn listed_capability(capability: ToolRecoveryCapability) -> usize {
        match capability {
            ToolRecoveryCapability::NonRecoverable => 0,
            ToolRecoveryCapability::ReplaySafe => 1,
            ToolRecoveryCapability::Idempotent => 2,
            ToolRecoveryCapability::DurableRequest => 3,
        }
    }
    const EVERY_CAPABILITY: [ToolRecoveryCapability; 4] = [
        ToolRecoveryCapability::NonRecoverable,
        ToolRecoveryCapability::ReplaySafe,
        ToolRecoveryCapability::Idempotent,
        ToolRecoveryCapability::DurableRequest,
    ];

    const fn listed_batch_phase(phase: ToolBatchPhase) -> usize {
        match phase {
            ToolBatchPhase::Open => 0,
            ToolBatchPhase::Finalized => 1,
        }
    }
    const EVERY_BATCH_PHASE: [ToolBatchPhase; 2] =
        [ToolBatchPhase::Open, ToolBatchPhase::Finalized];

    fn every_budget() -> impl Iterator<Item = NonZeroU16> {
        (1..=u16::MAX).map(|budget| NonZeroU16::new(budget).expect("the range starts at one"))
    }

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_WAIT_KIND.into_iter().enumerate() {
            assert_eq!(listed_wait_kind(value), index);
        }
        let mut phases = 0;
        for (index, value) in every_phase().enumerate() {
            assert_eq!(listed_phase(value), index);
            phases += 1;
        }
        assert_eq!(phases, EVERY_PHASE_COUNT);
        for (index, value) in EVERY_TRANSITION.into_iter().enumerate() {
            assert_eq!(listed_transition(value), index);
        }
        for (index, value) in EVERY_MODE.into_iter().enumerate() {
            assert_eq!(listed_mode(value), index);
        }
        for (index, value) in EVERY_CAPABILITY.into_iter().enumerate() {
            assert_eq!(listed_capability(value), index);
        }
        for (index, value) in EVERY_BATCH_PHASE.into_iter().enumerate() {
            assert_eq!(listed_batch_phase(value), index);
        }
    }

    /// @exhaustive NonZeroU16(65535) = 65535
    /// @obligation tool.transition_preconditions
    #[test]
    fn terminal_calls_are_never_reentered() {
        for budget in every_budget() {
            assert_eq!(
                next_execution_attempt(
                    CallPhaseView::Completed,
                    budget,
                    ExecutionRequest::StartOrRetry,
                ),
                Err(ToolBatchError::InvalidTransition)
            );
        }
    }

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation tool.transition_preconditions
    #[test]
    fn only_the_matching_approval_ticket_enters_execution() {
        for correct_kind in [false, true] {
            for correct_correlation in [false, true] {
                let actual_kind = if correct_kind {
                    ToolWaitKind::ToolPermission
                } else {
                    ToolWaitKind::Delegation
                };
                let entered = next_execution_attempt(
                    CallPhaseView::Awaiting(actual_kind),
                    NonZeroU16::MIN,
                    ExecutionRequest::Resume {
                        expected_kind: ToolWaitKind::ToolPermission,
                        correlation_matches: correct_correlation,
                    },
                )
                .is_ok();
                assert_eq!(entered, correct_kind && correct_correlation);
            }
        }
    }

    /// @exhaustive bool(2) x NonZeroU16(65535) x ToolRecoveryMode(4) x ToolRecoveryCapability(4) = 2097120
    /// @obligation tool.transition_preconditions
    #[test]
    fn known_returns_only_admit_materialization_and_never_physical_reentry() {
        // Complete new-state partition: pending Returned / terminal KnownAborted,
        // every nonzero u16 budget and every recovery capability. Neither obtains
        // a physical entry; only pending Returned admits materialization. Payload
        // equality/codec durability remain real aggregate/runtime test evidence.
        for pending in [false, true] {
            let phase = if pending {
                CallPhaseView::Returned
            } else {
                CallPhaseView::KnownAborted
            };
            assert_eq!(executor_entry_attempt(phase), None);
            assert_eq!(
                permits_call_transition(phase, CallTransition::MaterializeReturn),
                pending
            );
            assert!(!permits_call_transition(
                phase,
                CallTransition::MarkIndeterminate
            ));
            for budget in every_budget() {
                assert_eq!(
                    next_execution_attempt(phase, budget, ExecutionRequest::StartOrRetry),
                    Err(ToolBatchError::InvalidTransition)
                );
                for mode in EVERY_MODE {
                    let policy = ToolRecoveryPolicy::new(mode, budget);
                    for capability in EVERY_CAPABILITY {
                        assert_eq!(
                            recovery_transition(phase, &policy, capability),
                            Err(ToolBatchError::InvalidTransition)
                        );
                    }
                }
            }
        }
    }

    /// @exhaustive CallPhaseView(65545) = 65545
    /// @obligation tool.executor_entry_order
    #[test]
    fn executor_entry_admission_requires_exactly_executing_phase() {
        // Causes: every durable phase tag and every u16 attempt. Effect: the
        // production admission kernel returns that exact attempt iff the phase
        // is Executing.
        for phase in every_phase() {
            let admitted = executor_entry_attempt(phase);
            match phase {
                CallPhaseView::Executing(attempt) => assert_eq!(admitted, Some(attempt)),
                CallPhaseView::Requested
                | CallPhaseView::Awaiting(_)
                | CallPhaseView::Returned
                | CallPhaseView::KnownAborted
                | CallPhaseView::Completed
                | CallPhaseView::Indeterminate => assert_eq!(admitted, None),
            }
        }
    }

    /// @exhaustive CallPhaseView(65545) x CallTransition(6) = 393270
    /// @obligation tool.transition_preconditions
    #[test]
    fn every_tool_call_transition_has_the_unique_documented_precondition() {
        // Independent cause/effect table for first-application admission.
        // Columns are executor/wait completion, immediate completion, raw
        // return, materialization, unknown sealing, and staged publication.
        // Requested has no physical entry; Executing/Awaiting own one pending
        // outcome. Returned owns known raw and may only materialize it.
        // KnownAborted is absorbing and publishes nothing. Completed retains
        // only its prepared publication; Indeterminate retains its original
        // unknown-sealing and publication roles. Exact completion/return replay
        // is a separate aggregate rule, not first-application admission here.
        // No predicate below calls or restates the production transition guard.
        for phase in every_phase() {
            let allowed = match phase {
                CallPhaseView::Requested => [false, true, true, false, true, false],
                CallPhaseView::Executing(_) | CallPhaseView::Awaiting(_) => {
                    [true, false, true, false, true, false]
                }
                CallPhaseView::Returned => [false, false, false, true, false, false],
                CallPhaseView::KnownAborted => [false, false, false, false, false, false],
                CallPhaseView::Completed => [false, false, false, false, false, true],
                CallPhaseView::Indeterminate => [false, false, false, false, true, true],
            };
            for transition in EVERY_TRANSITION {
                let expected = match transition {
                    CallTransition::CompleteFromExecutionOrWait => allowed[0],
                    CallTransition::CompleteImmediate => allowed[1],
                    CallTransition::RecordReturn => allowed[2],
                    CallTransition::MaterializeReturn => allowed[3],
                    CallTransition::MarkIndeterminate => allowed[4],
                    CallTransition::StageResult => allowed[5],
                };
                assert_eq!(permits_call_transition(phase, transition), expected);
            }
        }
    }

    /// @exhaustive CallPhaseView(65545) x completion CallTransition(2) x bool(2) = 262180
    /// @obligation tool_result.call_is_consumed_at_most_once
    #[test]
    fn a_tool_call_completion_is_applied_at_most_once() {
        // Complete cause/effect partition: every durable call phase, both
        // completion sources and exact/changed replay. Apply is possible only
        // from the source's one legal nonterminal predecessor; Completed exact
        // is Replay, and every other terminal combination is Reject. Thus no
        // terminal predecessor can apply another result. This is the production
        // classifier called by both ToolBatch completion methods.
        for phase in every_phase() {
            for transition in [
                CallTransition::CompleteFromExecutionOrWait,
                CallTransition::CompleteImmediate,
            ] {
                for exact_completed_output in [false, true] {
                    let decision =
                        completion_disposition(phase, transition, exact_completed_output);
                    let expected = if phase == CallPhaseView::Completed && exact_completed_output {
                        CompletionDisposition::Replay
                    } else if permits_call_transition(phase, transition) {
                        CompletionDisposition::Apply
                    } else {
                        CompletionDisposition::Reject
                    };
                    assert_eq!(decision, expected);
                    if matches!(
                        phase,
                        CallPhaseView::Returned
                            | CallPhaseView::KnownAborted
                            | CallPhaseView::Completed
                            | CallPhaseView::Indeterminate
                    ) {
                        assert_ne!(decision, CompletionDisposition::Apply);
                    }
                }
            }
        }
    }

    /// @exhaustive terminal CallPhaseView(3) x CallTransition(6) = 18
    /// @obligation tool.transition_preconditions
    #[test]
    fn terminal_tool_calls_only_accept_result_staging() {
        for phase in [
            CallPhaseView::Completed,
            CallPhaseView::Indeterminate,
            CallPhaseView::KnownAborted,
        ] {
            for transition in EVERY_TRANSITION {
                if permits_call_transition(phase, transition) {
                    assert!(matches!(
                        transition,
                        CallTransition::MarkIndeterminate | CallTransition::StageResult
                    ));
                    if phase == CallPhaseView::Completed {
                        assert_eq!(transition, CallTransition::StageResult);
                    }
                }
            }
        }
    }

    /// @exhaustive CallPhaseView(65545) = 65545
    /// @obligation tool.transition_preconditions
    #[test]
    fn run_end_sealing_targets_exactly_nonterminal_calls() {
        for phase in every_phase() {
            assert_eq!(
                must_seal_on_run_end(phase),
                !matches!(
                    phase,
                    CallPhaseView::KnownAborted
                        | CallPhaseView::Completed
                        | CallPhaseView::Indeterminate
                )
            );
        }
    }

    /// @exhaustive CallPhaseView(65545) x bool(2) = 131090
    /// @obligation tool.transition_preconditions
    #[test]
    fn a_second_wait_is_never_admitted() {
        // Cause/effect decision table: K1 awaitable+no peer => admitted;
        // K2 awaitable+peer => rejected; K3 non-awaitable => rejected. Every
        // phase and peer occupancy is enumerated.
        for phase in every_phase() {
            for another_call_is_awaiting in [false, true] {
                let admitted = permits_await_transition(phase, another_call_is_awaiting);
                assert_eq!(
                    admitted,
                    !another_call_is_awaiting
                        && matches!(
                            phase,
                            CallPhaseView::Requested | CallPhaseView::Executing(_)
                        )
                );
                if another_call_is_awaiting {
                    assert!(!admitted);
                }
            }
        }
    }

    /// @exhaustive ToolBatchPhase(2) = 2
    /// @obligation tool.publication_barrier
    #[test]
    fn only_finalized_batches_expose_staged_publication() {
        // Causes: the complete ToolBatchPhase domain. Effect: the production
        // publication admission kernel accepts exactly Finalized. Completion
        // remains a premise established by `finalize` or persisted-state
        // validation; allocation, async commit and database durability are
        // outside this local phase proof. Decision rules: K1(Open) -> reject;
        // K2(Finalized) -> admit.
        for phase in EVERY_BATCH_PHASE {
            assert_eq!(
                permits_publication(phase),
                phase == ToolBatchPhase::Finalized
            );
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_recovery_mode(tag: u8) -> ToolRecoveryMode {
        match tag % 4 {
            0 => ToolRecoveryMode::NeverReplay,
            1 => ToolRecoveryMode::ReplaySafe,
            2 => ToolRecoveryMode::Idempotent,
            _ => ToolRecoveryMode::DurableRequest,
        }
    }

    fn symbolic_recovery_capability(tag: u8) -> ToolRecoveryCapability {
        match tag % 4 {
            0 => ToolRecoveryCapability::NonRecoverable,
            1 => ToolRecoveryCapability::ReplaySafe,
            2 => ToolRecoveryCapability::Idempotent,
            _ => ToolRecoveryCapability::DurableRequest,
        }
    }

    /// @obligation tool.recovery_policy
    #[kani::proof]
    fn recovery_reentry_requires_policy_capability_and_remaining_budget() {
        // Causes: all four policy modes, all four capability modes, both
        // recoverable durable phases, every u16 attempt, and every non-zero u16
        // budget. Effects: Requested enters attempt 1 exactly when supported;
        // Executing enters n+1 exactly for a supported replay mode with room;
        // every NeverReplay, unsupported, overflow, or exhausted case is the
        // matching fail-closed transition. Awaiting and both terminal phases
        // are additionally rejected. No assumptions or bounded collections are
        // needed because the production kernel is enum/integer-only.
        let mode = symbolic_recovery_mode(kani::any());
        let capability = symbolic_recovery_capability(kani::any());
        let budget = NonZeroU16::new(kani::any()).unwrap_or(NonZeroU16::MIN);
        let policy = ToolRecoveryPolicy::new(mode, budget);
        let executing = kani::any::<bool>();
        let attempt = kani::any::<u16>();
        let phase = if executing {
            CallPhaseView::Executing(attempt)
        } else {
            CallPhaseView::Requested
        };
        let actual = recovery_transition(phase, &policy, capability).unwrap();
        let supported = crate::tool::recovery_capability_expected(mode, capability);
        let expected = if !executing {
            if supported {
                RecoveryTransition::Enter(1)
            } else {
                RecoveryTransition::UnsupportedCapability
            }
        } else if mode == ToolRecoveryMode::NeverReplay {
            RecoveryTransition::NeverReplay
        } else if !supported {
            RecoveryTransition::UnsupportedCapability
        } else {
            match attempt.checked_add(1) {
                Some(next) if next <= budget.get() => RecoveryTransition::Enter(next),
                _ => RecoveryTransition::AttemptsExhausted,
            }
        };
        assert_eq!(actual, expected);

        for invalid in [
            CallPhaseView::Awaiting(ToolWaitKind::ExternalResult),
            CallPhaseView::Returned,
            CallPhaseView::KnownAborted,
            CallPhaseView::Completed,
            CallPhaseView::Indeterminate,
        ] {
            assert_eq!(
                recovery_transition(invalid, &policy, capability),
                Err(ToolBatchError::InvalidTransition)
            );
        }
    }
}
