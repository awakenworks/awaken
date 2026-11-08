// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The one rule settling a refused Session Run admission in Event-batch recovery: a
// @formal reason: permanent refusal enters the aggregate's terminal-failure command through the root CAS,
// @formal reason: a transient one stays queued. Classification, not a formal-proof claim.
//! How Event-batch recovery settles a User Run it could not admit.
//!
//! Lifecycle recovery retries an accepted Event batch until its Run is
//! admitted. A refusal no retry can admit (a bad request from projection or
//! delivery, a classified invariant, a missing pinned publication) must not
//! re-queue forever: the Session's frozen inputs can no longer admit
//! execution, so the Session fails terminally with that refusal as its
//! durable cause. Its accepted batches then resolve through the terminal
//! root, and the protocol projects one terminal error.

use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::SessionExecutionState;
use awaken_session_execution_contract::{
    SessionRealizationControlDisposition, effect_failure_disposition,
};
use awaken_time::WallClockExt as _;

use super::{EventBatchProgress, SessionEventBatchCommands, mutation_run_error};
use crate::SessionMutationError;
use crate::mutation::ROOT_CAS_ATTEMPTS;

/// The settlement of one refused User Run admission or activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefusedRunSettlement {
    /// A retry may admit it: the batch stays queued and the scan backs off.
    Retry,
    /// No retry can admit it: the Session fails terminally with its cause.
    FailSession,
    /// The root is already terminal; its batches resolve through it.
    Terminal,
}

/// The one rule for a refused User Run. The refusal's retry class is
/// [`effect_failure_disposition`], the classification every Session effect
/// failure already shares; this rule adds only the Session's state.
///
/// Decision table (causes → effect):
/// | Rule | refusal class | Session execution | effect |
/// |---|---|---|---|
/// | R1 | NotReady or Retryable (Unavailable, Conflict, unclassified internal), or the Session's budget pause (`budget_reached`, which an allowance raise lifts) | live | `Retry`: queued, retried with the recovery backoff |
/// | R2 | Terminal (BadRequest, classified internal, permanent conflict) | live (Preparing, Activating, Running, Rescheduling, Idle) | `FailSession`: ActivationFailed carrying the refusal; one terminal `session.error`; status terminated; batches resolve terminally |
/// | R3 | any | terminal | `Terminal`: nothing to fail; the terminal root resolves its batches |
pub(super) fn refused_run_settlement(
    error: &RunError,
    execution: SessionExecutionState,
) -> RefusedRunSettlement {
    if execution.is_terminal() {
        return RefusedRunSettlement::Terminal;
    }
    // A budget pause is a wait, not a refusal: the batch is retained until
    // the allowance admits it.
    if error.code == crate::SessionActivityError::BudgetReached.run_error().code {
        return RefusedRunSettlement::Retry;
    }
    match effect_failure_disposition(error) {
        SessionRealizationControlDisposition::NotReady
        | SessionRealizationControlDisposition::Retryable => RefusedRunSettlement::Retry,
        SessionRealizationControlDisposition::Terminal => RefusedRunSettlement::FailSession,
    }
}

impl SessionEventBatchCommands {
    /// Settle the refusal of `session`'s User Run by
    /// [`refused_run_settlement`]. `Retry` returns the refusal unchanged;
    /// otherwise the batch selector advances to the terminal root.
    pub(super) async fn settle_refused_user_run(
        &self,
        session_id: &awaken_session_types::SessionId,
        refusal: RunError,
    ) -> Result<EventBatchProgress, RunError> {
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let (owner_scope, mut session) = self.load_event_root(session_id).await?;
            match refused_run_settlement(&refusal, session.execution) {
                RefusedRunSettlement::Retry => return Err(refusal),
                RefusedRunSettlement::Terminal => return Ok(EventBatchProgress::Advanced),
                RefusedRunSettlement::FailSession => {}
            }
            // No Run executed the refused admission, so no Run terminal
            // sources this failure: the projection reports the refusal itself.
            session
                .lifecycle()
                .fail_terminally_at(refusal.to_string(), self.clock.now_ms(), None)
                .map_err(|error| RunError::internal(error.to_string()))?;
            match self
                .mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "fail-session-on-refused-run",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    tracing::warn!(
                        %session_id,
                        error = %refusal,
                        "Session failed terminally: its Run admission was refused permanently"
                    );
                    self.lifecycle.notify_fact();
                    return Ok(EventBatchProgress::Advanced);
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(mutation_run_error(error)),
            }
        }
        Err(RunError::unavailable(
            "terminal failure of a refused Session Run conflicted repeatedly",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_run_settlement_follows_its_decision_table() {
        use SessionExecutionState as State;
        let live = [
            State::Preparing,
            State::Activating,
            State::Running,
            State::Rescheduling,
            State::Idle,
        ];
        let retryable = [
            RunError::unavailable("store unavailable"),
            RunError::unavailable_classified(
                "session_projection_not_installed",
                "no installed projection",
            ),
            RunError::unavailable_classified(
                awaken_session_execution_contract::ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                "no room",
            ),
            RunError::conflict("lost a race"),
            RunError::internal("unclassified fault"),
            crate::SessionActivityError::BudgetReached.run_error(),
        ];
        let permanent = [
            RunError::bad_request("a Worker-placed Session requires a pinned Agent publication"),
            RunError::classified(
                "session_realization_invariant_violated",
                "frozen Session Agent publication is unavailable",
            ),
            RunError::conflict_classified(
                awaken_session_execution_contract::SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE,
                "nothing to rebuild from",
            ),
        ];
        for execution in live {
            for error in &retryable {
                assert_eq!(
                    refused_run_settlement(error, execution),
                    RefusedRunSettlement::Retry,
                    "R1 {execution:?} {error}"
                );
            }
            for error in &permanent {
                assert_eq!(
                    refused_run_settlement(error, execution),
                    RefusedRunSettlement::FailSession,
                    "R2 {execution:?} {error}"
                );
            }
        }
        for execution in [State::ActivationFailed, State::Terminated] {
            for error in retryable.iter().chain(&permanent) {
                assert_eq!(
                    refused_run_settlement(error, execution),
                    RefusedRunSettlement::Terminal,
                    "R3 {execution:?} {error}"
                );
            }
        }
    }
}
