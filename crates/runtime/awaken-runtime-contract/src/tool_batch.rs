//! Durable state of one model-emitted tool-call batch.
//!
//! The transcript is exposed to the model only after every call has a terminal
//! result, but each call's execution outcome is committed independently. This
//! cell is the recovery truth; futures, abort handles, and worker tasks are not.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use serde::{Deserialize, Serialize};

use crate::llm::ToolCall;
use crate::tool::{ToolOutput, ToolRecoveryPolicy};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBatchId(pub String);

impl ToolBatchId {
    #[must_use]
    pub fn for_step(run_id: &RunId, step: usize) -> Self {
        Self(format!("tool-batch:{}:{step}", run_id.0))
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

/// The typed reason one call is waiting. Approval is deliberately not encoded as
/// a generic message: it must match the Run's committed ResumeTicket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolWaitKind {
    Approval,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatch {
    pub id: ToolBatchId,
    pub run_id: RunId,
    pub calls: Vec<DurableToolCall>,
    pub phase: ToolBatchPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolBatchError {
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
    #[error("tool batch still contains non-terminal calls")]
    Incomplete,
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
    Await,
    CompleteFromExecutionOrWait,
    CompleteImmediate,
    MarkIndeterminate,
    StageResult,
}

const fn permits_call_transition(phase: CallPhaseView, transition: CallTransition) -> bool {
    match transition {
        CallTransition::Await => matches!(
            phase,
            CallPhaseView::Requested | CallPhaseView::Executing(_)
        ),
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
    max_attempts: u16,
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
    if attempt > max_attempts {
        Err(ToolBatchError::AttemptsExhausted)
    } else {
        Ok(attempt)
    }
}

impl ToolBatch {
    pub fn new(
        id: ToolBatchId,
        run_id: RunId,
        calls: impl IntoIterator<Item = (ToolCall, ToolRecoveryPolicy)>,
    ) -> Result<Self, ToolBatchError> {
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
    pub fn is_complete(&self) -> bool {
        self.calls.iter().all(|call| call.phase.is_terminal())
    }

    pub fn mark_executing(&mut self, call_id: &str) -> Result<u16, ToolBatchError> {
        self.ensure_open()?;
        let call = self.call_mut(call_id)?;
        let phase = phase_view(&call.phase);
        let attempt = next_execution_attempt(
            phase,
            call.recovery_policy.max_attempts,
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
            call.recovery_policy.max_attempts,
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
        let call = self.call_mut(call_id)?;
        if !permits_call_transition(phase_view(&call.phase), CallTransition::Await) {
            return Err(ToolBatchError::InvalidTransition);
        }
        call.phase = ToolCallPhase::Awaiting {
            wait: ToolWait {
                kind,
                correlation_id: correlation_id.into(),
            },
        };
        Ok(())
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
    use crate::tool::ToolRecoveryMode;

    fn batch() -> ToolBatch {
        ToolBatch::new(
            ToolBatchId("b".into()),
            RunId("r".into()),
            [(
                ToolCall {
                    call_id: "c".into(),
                    tool_id: "t".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy {
                    mode: ToolRecoveryMode::ReplaySafe,
                    ..ToolRecoveryPolicy::default()
                },
            )],
        )
        .unwrap()
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
            .mark_awaiting("c", ToolWaitKind::Approval, "approval-1")
            .unwrap();
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::Approval, "stale"),
            Err(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::Delegation, "approval-1"),
            Err(ToolBatchError::WaitMismatch)
        );
        assert_eq!(
            batch.resume_executing("c", ToolWaitKind::Approval, "approval-1"),
            Ok(1)
        );
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
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn terminal_calls_are_never_reentered() {
        assert_eq!(
            next_execution_attempt(
                CallPhaseView::Completed,
                kani::any(),
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
            ToolWaitKind::Approval
        } else {
            ToolWaitKind::Delegation
        };
        let entered = next_execution_attempt(
            CallPhaseView::Awaiting(actual_kind),
            1,
            ExecutionRequest::Resume {
                expected_kind: ToolWaitKind::Approval,
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
            2 => CallPhaseView::Awaiting(ToolWaitKind::Approval),
            3 => CallPhaseView::Completed,
            _ => CallPhaseView::Indeterminate,
        }
    }

    fn symbolic_transition(tag: u8) -> CallTransition {
        match tag % 5 {
            0 => CallTransition::Await,
            1 => CallTransition::CompleteFromExecutionOrWait,
            2 => CallTransition::CompleteImmediate,
            3 => CallTransition::MarkIndeterminate,
            _ => CallTransition::StageResult,
        }
    }

    #[kani::proof]
    fn every_tool_call_transition_has_the_unique_documented_precondition() {
        let phase = symbolic_phase(kani::any());
        let transition = symbolic_transition(kani::any());
        let expected = match transition {
            CallTransition::Await => {
                matches!(
                    phase,
                    CallPhaseView::Requested | CallPhaseView::Executing(_)
                )
            }
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
}
