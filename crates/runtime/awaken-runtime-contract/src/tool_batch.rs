//! Durable state of one model-emitted tool-call batch.
//!
//! The transcript is exposed to the model only after every call has a terminal
//! result, but each call's execution outcome is committed independently. This
//! cell is the recovery truth; futures, abort handles, and worker tasks are not.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, ResumeTicket, ToolAwaitReason};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::num::NonZeroU16;

use crate::llm::ToolCall;
use crate::tool::{ToolOutput, ToolRecoveryPolicy};

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
    /// Terminal output committed before transcript publication.
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
        matches!(self, Self::Completed(_) | Self::Indeterminate { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableToolCall {
    pub call: ToolCall,
    pub recovery_policy: ToolRecoveryPolicy,
    pub phase: ToolCallPhase,
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

/// Validated aggregate for a non-empty, uniquely identified tool-call batch.
/// Its storage is private so callers cannot insert duplicate calls, reopen a
/// finalized batch, or publish staged effects before a terminal result.
///
/// ```compile_fail
/// use awaken_runtime_contract::tool_batch::{ToolBatch, ToolBatchPhase, ToolBatchId};
/// use awaken_runtime_contract::RunId;
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
    MarkIndeterminate,
    StageResult,
}

const fn permits_call_transition(phase: CallPhaseView, transition: CallTransition) -> bool {
    match transition {
        CallTransition::CompleteFromExecutionOrWait => matches!(
            phase,
            CallPhaseView::Executing(_) | CallPhaseView::Awaiting(_)
        ),
        CallTransition::CompleteImmediate => matches!(phase, CallPhaseView::Requested),
        CallTransition::MarkIndeterminate => !matches!(phase, CallPhaseView::Completed),
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

impl From<ToolAwaitReason> for ToolWaitKind {
    fn from(reason: ToolAwaitReason) -> Self {
        match reason {
            ToolAwaitReason::Permission => Self::ToolPermission,
            ToolAwaitReason::ClientExecution => Self::ExternalResult,
            ToolAwaitReason::ScheduledAction => Self::ScheduledAction,
            ToolAwaitReason::Delegation => Self::Delegation,
        }
    }
}

const fn must_seal_on_run_end(phase: CallPhaseView) -> bool {
    !matches!(
        phase,
        CallPhaseView::Completed | CallPhaseView::Indeterminate
    )
}

fn phase_view(phase: &ToolCallPhase) -> CallPhaseView {
    match phase {
        ToolCallPhase::Requested => CallPhaseView::Requested,
        ToolCallPhase::Executing { attempt } => CallPhaseView::Executing(*attempt),
        ToolCallPhase::Awaiting { wait } => CallPhaseView::Awaiting(wait.kind),
        ToolCallPhase::Completed(_) => CallPhaseView::Completed,
        ToolCallPhase::Indeterminate { .. } => CallPhaseView::Indeterminate,
    }
}

/// Heap-free production transition kernel shared by the aggregate methods and
/// Kani. It is the single authority for entering an executor attempt.
fn next_execution_attempt(
    phase: CallPhaseView,
    max_attempts: NonZeroU16,
    request: ExecutionRequest,
) -> Result<u16, ToolBatchError> {
    let attempt = match (phase, request) {
        (CallPhaseView::Requested, ExecutionRequest::StartOrRetry) => 1,
        (CallPhaseView::Executing(attempt), ExecutionRequest::StartOrRetry) => attempt
            .checked_add(1)
            .ok_or(ToolBatchError::AttemptsExhausted)?,
        (
            CallPhaseView::Awaiting(actual_kind),
            ExecutionRequest::Resume {
                expected_kind,
                correlation_matches: true,
            },
        ) if actual_kind == expected_kind => 1,
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
                ToolCallPhase::Completed(output) if output.call_id != *call_id => {
                    return Err(ToolBatchError::InvalidPersistedState(format!(
                        "call {call_id} contains another call's output"
                    )));
                }
                _ => {}
            }
            if (!entry.result_messages.is_empty() || !entry.result_state.is_empty())
                && !entry.phase.is_terminal()
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

    pub fn mark_executing(&mut self, call_id: &str) -> Result<u16, ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let phase = phase_view(&call.phase);
        let attempt = next_execution_attempt(
            phase,
            call.recovery_policy.max_attempts(),
            ExecutionRequest::StartOrRetry,
        )?;
        call.phase = ToolCallPhase::Executing { attempt };
        Ok(attempt)
    }

    /// Resume a typed durable wait into execution. Both the wait kind and its
    /// correlation must match the committed ticket; an ordinary message or a
    /// stale approval can never move the call into `Executing`.
    pub fn resume_executing(
        &mut self,
        call_id: &str,
        expected_kind: ToolWaitKind,
        correlation_id: &str,
    ) -> Result<u16, ToolBatchError> {
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
        call.phase = ToolCallPhase::Executing { attempt };
        Ok(attempt)
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
    pub fn validate_awaiting_ticket<'a>(
        &'a self,
        expected_thread: &ThreadId,
        ticket: &ResumeTicket,
    ) -> Result<&'a DurableToolCall, ToolBatchWaitError> {
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
        } = ticket.target()
        else {
            return Err(ToolBatchWaitError::NotToolCall);
        };
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
        if wait.kind != (*reason).into() {
            return Err(ToolBatchWaitError::KindMismatch);
        }
        if wait.correlation_id != ticket.correlation_id {
            return Err(ToolBatchWaitError::CorrelationMismatch);
        }
        if entry.call.tool_id != tool.tool_id || entry.call.arguments != tool.arguments {
            return Err(ToolBatchWaitError::ToolMismatch);
        }
        Ok(entry)
    }

    pub fn complete(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(&output.call_id)?;
        match &call.phase {
            ToolCallPhase::Completed(existing) if existing == &output => return Ok(()),
            phase
                if permits_call_transition(
                    phase_view(phase),
                    CallTransition::CompleteFromExecutionOrWait,
                ) => {}
            _ => {
                return Err(ToolBatchError::InvalidTransition);
            }
        }
        call.phase = ToolCallPhase::Completed(output);
        Ok(())
    }

    /// Complete a call that the runtime answered without entering an external
    /// executor (schema/open, policy block, or a gate-supplied result).
    pub fn complete_immediate(&mut self, output: ToolOutput) -> Result<(), ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(&output.call_id)?;
        match &call.phase {
            ToolCallPhase::Completed(existing) if existing == &output => return Ok(()),
            phase
                if permits_call_transition(
                    phase_view(phase),
                    CallTransition::CompleteImmediate,
                ) => {}
            _ => return Err(ToolBatchError::InvalidTransition),
        }
        call.phase = ToolCallPhase::Completed(output);
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

    /// Seal every unfinished call when its owning Run ends.
    ///
    /// A terminal Run can never be resumed, so retaining a `Requested`,
    /// `Executing`, or `Awaiting` call would leave an impossible recovery
    /// instruction in durable state. Completed results are preserved; every
    /// unfinished call becomes indeterminate and the batch is finalized.
    /// Reapplying this operation is an idempotent no-op.
    pub fn seal_on_run_end(&mut self, reason: &str) {
        if self.phase == ToolBatchPhase::Finalized {
            return;
        }
        for call in &mut self.calls {
            if must_seal_on_run_end(phase_view(&call.phase)) {
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

    fn batch() -> ToolBatch {
        ToolBatch::for_step(
            RunId("r".into()),
            0,
            [(
                ToolCall {
                    call_id: "c".into(),
                    tool_id: "t".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::replay_safe(),
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
    fn terminal_output_is_idempotent_and_finalization_requires_completion() {
        let mut batch = batch();
        assert_eq!(batch.finalize(), Err(ToolBatchError::Incomplete));
        batch.mark_executing("c").unwrap();
        let output = ToolOutput::ok("c", "done");
        batch.complete(output.clone()).unwrap();
        batch.complete(output).unwrap();
        batch.finalize().unwrap();
        assert_eq!(batch.phase, ToolBatchPhase::Finalized);
        assert_eq!(batch.mark_executing("c"), Err(ToolBatchError::Finalized));
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

    #[test]
    fn approval_must_match_the_committed_wait() {
        let mut batch = batch();
        batch
            .mark_awaiting("c", ToolWaitKind::ToolPermission, "approval-1")
            .unwrap();
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::ToolPermission, "stale"),
            Err(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::Delegation, "approval-1"),
            Err(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::ToolPermission, "approval-1"),
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
        // C9 tool id and arguments match. Effect E1 is the sole awaiting entry;
        // each negated cause fails closed with its specific error and no state
        // transition. The decision table has one success rule A1=C1..C9=>E1
        // and one MC/DC rule A2..A10 for each independently negated cause.
        let mut batch = two_call_batch();
        batch
            .mark_awaiting("c1", ToolWaitKind::ToolPermission, "corr")
            .unwrap();
        let exact = tool_ticket(ToolAwaitReason::Permission, "c1", "tool-c1", "corr");
        assert_eq!(
            batch
                .validate_awaiting_ticket(&ThreadId("thread".into()), &exact)
                .unwrap()
                .call
                .call_id,
            "c1",
            "A1/E1"
        );

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
        let mut batch = batch();
        batch.mark_executing("c").unwrap();
        batch.complete(ToolOutput::ok("c", "ok")).unwrap();
        let mut value = serde_json::to_value(batch).unwrap();
        value["calls"][0]["phase"]["Completed"]["call_id"] = serde_json::json!("different-call");
        assert!(serde_json::from_value::<ToolBatch>(value).is_err());
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn terminal_calls_are_never_reentered() {
        let budget = NonZeroU16::new(kani::any()).unwrap_or(NonZeroU16::MIN);
        assert_eq!(
            next_execution_attempt(
                CallPhaseView::Completed,
                budget,
                ExecutionRequest::StartOrRetry,
            ),
            Err(ToolBatchError::InvalidTransition)
        );
    }

    #[kani::proof]
    fn only_the_matching_approval_ticket_enters_execution() {
        let correct_kind: bool = kani::any();
        let correct_correlation: bool = kani::any();
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

    fn symbolic_phase(tag: u8) -> CallPhaseView {
        match tag % 5 {
            0 => CallPhaseView::Requested,
            1 => CallPhaseView::Executing(1),
            2 => CallPhaseView::Awaiting(ToolWaitKind::ToolPermission),
            3 => CallPhaseView::Completed,
            _ => CallPhaseView::Indeterminate,
        }
    }

    fn symbolic_transition(tag: u8) -> CallTransition {
        match tag % 4 {
            0 => CallTransition::CompleteFromExecutionOrWait,
            1 => CallTransition::CompleteImmediate,
            2 => CallTransition::MarkIndeterminate,
            _ => CallTransition::StageResult,
        }
    }

    #[kani::proof]
    fn every_tool_call_transition_has_the_unique_documented_precondition() {
        let phase = symbolic_phase(kani::any());
        let transition = symbolic_transition(kani::any());
        let expected = match transition {
            CallTransition::CompleteFromExecutionOrWait => {
                matches!(
                    phase,
                    CallPhaseView::Executing(_) | CallPhaseView::Awaiting(_)
                )
            }
            CallTransition::CompleteImmediate => matches!(phase, CallPhaseView::Requested),
            CallTransition::MarkIndeterminate => !matches!(phase, CallPhaseView::Completed),
            CallTransition::StageResult => matches!(
                phase,
                CallPhaseView::Completed | CallPhaseView::Indeterminate
            ),
        };
        assert_eq!(permits_call_transition(phase, transition), expected);
    }

    #[kani::proof]
    fn terminal_tool_calls_only_accept_result_staging() {
        let phase = if kani::any() {
            CallPhaseView::Completed
        } else {
            CallPhaseView::Indeterminate
        };
        let transition = symbolic_transition(kani::any());
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

    #[kani::proof]
    fn run_end_sealing_targets_exactly_nonterminal_calls() {
        let phase = symbolic_phase(kani::any());
        assert_eq!(
            must_seal_on_run_end(phase),
            !matches!(
                phase,
                CallPhaseView::Completed | CallPhaseView::Indeterminate
            )
        );
    }

    #[kani::proof]
    fn a_second_wait_is_never_admitted() {
        // Cause/effect decision table: K1 awaitable+no peer => admitted;
        // K2 awaitable+peer => rejected; K3 non-awaitable => rejected. Symbolic
        // phase and peer occupancy cover every combination.
        let phase = symbolic_phase(kani::any());
        let another_call_is_awaiting = kani::any::<bool>();
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
