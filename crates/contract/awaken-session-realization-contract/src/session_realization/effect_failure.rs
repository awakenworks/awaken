// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Closed classification of a failed Session realization effect (ADR-0092).
//!
//! The realization driver reports a failed effect to Control and returns it to
//! the Worker, which projects it to a claim outcome. Both read this one
//! classification, so they cannot disagree about the same failure; the
//! Session aggregate's retry-budget decision for the report lives here too.

use crate::{FailSessionRealization, RunError, SessionRealizationControlDisposition};
use awaken_session_state_contract::{PersistedSession, SessionExecutionState};

/// Stable `RunError` code of a Transient environment failure: the provider
/// mechanism or a transitional state failed and an identical retry may clear.
pub const ENVIRONMENT_UNAVAILABLE_CODE: &str = "environment_unavailable";
/// Stable `RunError` code of an environment backend that had no room for the
/// request: nothing was realized and another Worker, a raised quota or a new
/// node may succeed.
pub const ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE: &str = "environment_capacity_unavailable";
/// Stable `RunError` code of a Permanent environment failure: an identical
/// retry against the same provider cannot succeed.
pub const ENVIRONMENT_REJECTED_CODE: &str = "environment_rejected";

/// Carry one provider failure across the environment realization boundary
/// with its disposition: Transient stays retryable, Permanent is terminal and
/// a capacity refusal keeps its own class.
#[must_use]
pub fn environment_effect_error(error: &awaken_provisioning_contract::SandboxError) -> RunError {
    use awaken_agent_contract::resilience::{Classify, Disposition};
    let message = error.to_string();
    if matches!(
        error,
        awaken_provisioning_contract::SandboxError::CapacityUnavailable(_)
    ) {
        return RunError::unavailable_classified(ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, message);
    }
    match error.disposition() {
        Disposition::Transient | Disposition::Quota { .. } => {
            RunError::unavailable_classified(ENVIRONMENT_UNAVAILABLE_CODE, message)
        }
        Disposition::Permanent => RunError::classified(ENVIRONMENT_REJECTED_CODE, message),
    }
}

/// Whether `code` was produced by [`environment_effect_error`].
#[must_use]
pub fn is_environment_effect_code(code: &str) -> bool {
    matches!(
        code,
        ENVIRONMENT_UNAVAILABLE_CODE
            | ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE
            | ENVIRONMENT_REJECTED_CODE
    )
}

/// The Worker effect class of one failed realization effect, by its typed
/// fault and stable code, never its message: a capacity refusal is `NotReady`
/// (the request-local deferral that keeps every retry budget), any other
/// Unavailable fault is `Retryable`, everything else is `Terminal`. A capacity
/// code on a non-Unavailable fault fails closed to `Terminal`.
#[must_use]
pub fn effect_failure_disposition(error: &RunError) -> SessionRealizationControlDisposition {
    match error.kind {
        awaken_session_types::RunErrorKind::Unavailable
            if error.code == ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE =>
        {
            SessionRealizationControlDisposition::NotReady
        }
        awaken_session_types::RunErrorKind::Unavailable => {
            SessionRealizationControlDisposition::Retryable
        }
        awaken_session_types::RunErrorKind::Internal
        | awaken_session_types::RunErrorKind::BadRequest => {
            SessionRealizationControlDisposition::Terminal
        }
    }
}

/// The Session aggregate's decision for one exact, current realization
/// failure of an initial (non-Idle) realization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealizationFailureOutcome {
    /// Keep the execution state and expire the lease; `refund_attempt` returns
    /// the attempt the lease's assignment counted (no environment was made).
    Retry { refund_attempt: bool },
    /// Enter the terminal realization failure.
    Absorb,
}

/// A capacity refusal never consumes budget and so never exhausts it; a
/// capacity flag on a non-retryable report is malformed and fails closed.
const fn realization_failure_outcome(
    retryable: bool,
    capacity_unavailable: bool,
    execution: SessionExecutionState,
    attempts: u32,
    budget: u32,
) -> Result<RealizationFailureOutcome, &'static str> {
    if capacity_unavailable && !retryable {
        return Err("a capacity failure must be retryable");
    }
    // Only an assignment in Preparing or Activating counted an attempt.
    let assignment_counted = matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Activating
    );
    Ok(if capacity_unavailable {
        RealizationFailureOutcome::Retry {
            refund_attempt: assignment_counted && attempts > 0,
        }
    } else if retryable && attempts < budget {
        RealizationFailureOutcome::Retry {
            refund_attempt: false,
        }
    } else {
        RealizationFailureOutcome::Absorb
    })
}

impl FailSessionRealization {
    /// The report for one failed effect: its retry class and whether it was a
    /// capacity refusal, from [`effect_failure_disposition`].
    #[must_use]
    pub fn for_effect(
        session_id: crate::SessionId,
        lease: crate::SessionRealizationLease,
        prepared_resource_revision: Option<u64>,
        source_run_id: Option<awaken_agent_contract::agent::run::Id>,
        error: &RunError,
    ) -> Self {
        let disposition = effect_failure_disposition(error);
        Self {
            session_id,
            lease,
            prepared_resource_revision,
            retryable: disposition != SessionRealizationControlDisposition::Terminal,
            capacity_unavailable: disposition == SessionRealizationControlDisposition::NotReady,
            source_run_id,
            reason: error.to_string(),
        }
    }

    /// Classify the current root without mutating its compatible DTO. The
    /// controlled aggregate applies any counted-attempt refund separately.
    pub fn initial_realization_failure_plan(
        &self,
        session: &PersistedSession,
        budget: u32,
    ) -> Result<RealizationFailureOutcome, crate::SessionRealizationControlFailure> {
        let outcome = realization_failure_outcome(
            self.retryable,
            self.capacity_unavailable,
            session.execution,
            session.realization_progress.attempts,
            budget,
        )
        .map_err(|reason| crate::SessionRealizationControlFailure::Invalid(reason.into()))?;
        if session.execution == SessionExecutionState::Idle {
            return Ok(RealizationFailureOutcome::Absorb);
        }
        Ok(outcome)
    }

    /// Whether this failure retries the Session's initial realization (Idle
    /// Sessions answer `false` and keep their existing path). A capacity
    /// refusal retries and refunds the attempt its lease's assignment counted
    /// (Preparing or Activating), so it never spends the persisted budget.
    pub fn retries_initial_realization(
        &self,
        session: &mut PersistedSession,
        budget: u32,
    ) -> Result<bool, crate::SessionRealizationControlFailure> {
        let outcome = self.initial_realization_failure_plan(session, budget)?;
        Ok(match outcome {
            RealizationFailureOutcome::Retry { refund_attempt } => {
                if refund_attempt {
                    session.realization_progress.attempts -= 1;
                }
                true
            }
            RealizationFailureOutcome::Absorb => false,
        })
    }
}

#[cfg(kani)]
fn symbolic_execution() -> SessionExecutionState {
    match kani::any::<u8>() % 7 {
        0 => SessionExecutionState::Preparing,
        1 => SessionExecutionState::Activating,
        2 => SessionExecutionState::ActivationFailed,
        3 => SessionExecutionState::Running,
        4 => SessionExecutionState::Rescheduling,
        5 => SessionExecutionState::Idle,
        _ => SessionExecutionState::Terminated,
    }
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn capacity_failures_never_consume_or_exhaust_the_realization_budget() {
    let retryable: bool = kani::any();
    let capacity: bool = kani::any();
    let execution = symbolic_execution();
    let attempts: u32 = kani::any();
    let budget: u32 = kani::any();
    let counted = matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Activating
    );
    match realization_failure_outcome(retryable, capacity, execution, attempts, budget) {
        Err(_) => assert!(capacity && !retryable),
        Ok(RealizationFailureOutcome::Retry { refund_attempt }) => {
            assert!(retryable);
            assert_eq!(refund_attempt, capacity && counted && attempts > 0);
            assert!(capacity || attempts < budget);
        }
        Ok(RealizationFailureOutcome::Absorb) => {
            assert!(!capacity);
            assert!(!retryable || attempts >= budget);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_failures_classify_by_fault_and_code_and_report_capacity() {
        // Causes: C1 the effect's fault (Unavailable / Internal / BadRequest);
        // C2 its code is the capacity code. Effects: E1 the Worker disposition;
        // E2 the FailSessionRealization (retryable, capacity_unavailable).
        // | Rule | C1 | C2 | E1 | E2 |
        // | F1 | Unavailable | yes | NotReady | (true, true) |
        // | F2 | Unavailable | no | Retryable | (true, false) |
        // | F3 | Internal/BadRequest | any | Terminal | (false, false) |
        use awaken_provisioning_contract::SandboxError;
        let capacity =
            environment_effect_error(&SandboxError::capacity_unavailable("exceeded quota"));
        let rows = [
            (
                "F1",
                capacity,
                SessionRealizationControlDisposition::NotReady,
                (true, true),
            ),
            (
                "F2",
                environment_effect_error(&SandboxError::timeout("t")),
                SessionRealizationControlDisposition::Retryable,
                (true, false),
            ),
            (
                "F2",
                RunError::unavailable("u"),
                SessionRealizationControlDisposition::Retryable,
                (true, false),
            ),
            (
                "F3",
                environment_effect_error(&SandboxError::unauthorized("u")),
                SessionRealizationControlDisposition::Terminal,
                (false, false),
            ),
            (
                "F3",
                RunError::classified(ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, "mislabelled"),
                SessionRealizationControlDisposition::Terminal,
                (false, false),
            ),
            (
                "F3",
                RunError::bad_request_classified(ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, "bad"),
                SessionRealizationControlDisposition::Terminal,
                (false, false),
            ),
        ];
        let lease = crate::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/1".into(),
            epoch: 1,
            expires_at_unix_ms: 1,
        };
        for (rule, error, disposition, report) in rows {
            assert_eq!(effect_failure_disposition(&error), disposition, "{rule}");
            let command =
                FailSessionRealization::for_effect("s".into(), lease.clone(), None, None, &error);
            assert_eq!(
                (command.retryable, command.capacity_unavailable),
                report,
                "{rule}"
            );
            assert_eq!(command.reason, error.to_string(), "{rule}");
            assert!(
                is_environment_effect_code(&error.code)
                    == [
                        ENVIRONMENT_UNAVAILABLE_CODE,
                        ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                        ENVIRONMENT_REJECTED_CODE
                    ]
                    .contains(&error.code.as_str()),
                "{rule}"
            );
        }
    }

    #[test]
    fn effect_failure_disposition_is_exhaustive_over_fault_and_code() {
        // Exhaustive decision table over the classification's whole input
        // domain that matters: every RunErrorKind x {each stable environment
        // code, a foreign code}. Oracle derived from the row, not the table:
        // only Unavailable + capacity code defers; any other Unavailable
        // retries; Internal/BadRequest are terminal whatever the code.
        use awaken_session_types::RunErrorKind;
        for kind in [
            RunErrorKind::Unavailable,
            RunErrorKind::Internal,
            RunErrorKind::BadRequest,
        ] {
            for code in [
                ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                ENVIRONMENT_UNAVAILABLE_CODE,
                ENVIRONMENT_REJECTED_CODE,
                "unavailable",
                "",
            ] {
                let error = RunError {
                    message: "m".into(),
                    kind,
                    code: code.into(),
                };
                let expected = match (kind, code == ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE) {
                    (RunErrorKind::Unavailable, true) => {
                        SessionRealizationControlDisposition::NotReady
                    }
                    (RunErrorKind::Unavailable, false) => {
                        SessionRealizationControlDisposition::Retryable
                    }
                    _ => SessionRealizationControlDisposition::Terminal,
                };
                assert_eq!(
                    effect_failure_disposition(&error),
                    expected,
                    "{kind:?} {code}"
                );
            }
        }
    }

    #[test]
    fn capacity_failures_retry_without_consuming_and_others_keep_the_budget() {
        // Causes: C1 retryable; C2 capacity refusal (ADR-0092); C3 the lease's
        // assignment counted an attempt; C4 attempts below/at the budget.
        // Effects: E1 retry keeping the attempt; E2 retry refunding it; E3
        // absorb; E4 reject a malformed report.
        // | Rule | C1 | C2 | C3 | C4 | effect |
        // | B1 | yes | no | any | below | E1 |
        // | B2 | yes | no | any | at | E3 |
        // | B3 | no | no | any | any | E3 |
        // | B4 | yes | yes | yes | any (>0) | E2, even at/over budget |
        // | B5 | yes | yes | no | any | E1 (nothing was counted) |
        // | B6 | yes | yes | yes | 0 | E1 (never underflows) |
        // | B7 | no | yes | any | any | E4 fail closed |
        // C3 counted: Preparing/Activating; not counted: Running.
        use SessionExecutionState::{Activating, Preparing, Running};
        let budget = 3;
        let retry = |refund_attempt| Ok(RealizationFailureOutcome::Retry { refund_attempt });
        let absorb = Ok(RealizationFailureOutcome::Absorb);
        let malformed = Err("a capacity failure must be retryable");
        let rows = [
            ("B1", true, false, Preparing, 2, retry(false)),
            ("B1", true, false, Running, 0, retry(false)),
            ("B2", true, false, Activating, 3, absorb),
            ("B3", false, false, Preparing, 0, absorb),
            ("B3", false, false, Running, 3, absorb),
            ("B4", true, true, Preparing, 1, retry(true)),
            ("B4", true, true, Activating, 3, retry(true)),
            ("B4", true, true, Preparing, u32::MAX, retry(true)),
            ("B5", true, true, Running, 3, retry(false)),
            ("B6", true, true, Preparing, 0, retry(false)),
            ("B7", false, true, Preparing, 1, malformed),
        ];
        for (rule, retryable, capacity, execution, attempts, expected) in rows {
            assert_eq!(
                realization_failure_outcome(retryable, capacity, execution, attempts, budget),
                expected,
                "{rule}"
            );
        }
    }
}
