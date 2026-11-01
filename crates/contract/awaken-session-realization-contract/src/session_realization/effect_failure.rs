// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Closed classification of a failed Session realization effect (ADR-0092).
//!
//! The realization driver reports a failed effect to Control and returns it to
//! the Worker, which projects it to a claim outcome. Both read this one
//! classification, so they cannot disagree about the same failure; the
//! Session aggregate's retry-budget decision for the report lives here too.

use crate::{FailSessionRealization, RunError, SessionRealizationControlDisposition};
use awaken_session_contract::{PersistedSession, SessionExecutionState};

/// One failed realization effect across projection and driver boundaries.
/// The provider's relative scheduling hint travels beside the unchanged
/// neutral error. It is not a lease, a new provisioning deadline or a retry
/// budget; classification remains [`effect_failure_disposition`].
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct SessionRealizationEffectFailure {
    #[source]
    error: RunError,
    retry_after: Option<std::time::Duration>,
}

impl SessionRealizationEffectFailure {
    #[must_use]
    pub fn new(error: RunError, retry_after: Option<std::time::Duration>) -> Self {
        Self { error, retry_after }
    }

    #[must_use]
    pub fn run_error(&self) -> &RunError {
        &self.error
    }

    #[must_use]
    pub const fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }

    /// Charge only time the driver spent reporting this failure. No clock is
    /// read here and no Provider observation is reclassified; a spent hint is
    /// still present, as zero, so the existing pool immediately re-observes.
    #[must_use]
    pub fn after_wait(mut self, elapsed: std::time::Duration) -> Self {
        self.retry_after = retry_after_wait(self.retry_after, elapsed);
        self
    }
}

impl From<RunError> for SessionRealizationEffectFailure {
    fn from(error: RunError) -> Self {
        Self::new(error, None)
    }
}

impl From<SessionRealizationEffectFailure> for RunError {
    fn from(failure: SessionRealizationEffectFailure) -> Self {
        failure.error
    }
}

fn retry_after_wait(
    hint: Option<std::time::Duration>,
    elapsed: std::time::Duration,
) -> Option<std::time::Duration> {
    hint.map(|remaining| remaining.saturating_sub(elapsed))
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn driver_wait_never_extends_or_discards_the_original_retry_hint() {
    let duration = || std::time::Duration::new(kani::any(), kani::any::<u32>() % 1_000_000_000);
    let hint = kani::any::<bool>().then(duration);
    let elapsed = duration();
    let remaining = retry_after_wait(hint, elapsed);
    match (hint, remaining) {
        (None, None) => {}
        (Some(before), Some(after)) => {
            assert!(after <= before);
            assert_eq!(after, before.saturating_sub(elapsed));
            if elapsed >= before {
                assert!(after.is_zero());
            }
        }
        _ => unreachable!("waiting never changes hint presence"),
    }
}

/// Stable `RunError` code of a Transient environment failure: the provider
/// mechanism or a transitional state failed and an identical retry may clear.
pub const ENVIRONMENT_UNAVAILABLE_CODE: &str = "environment_unavailable";
/// Stable `RunError` code of an environment backend that had no room for the
/// request: nothing was realized and another Worker, a raised quota or a new
/// node may succeed.
pub const ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE: &str = "environment_capacity_unavailable";
/// Stable `RunError` code of an Environment the provider still reports
/// provisioning before the provider's own deadline: no failure yet, so it
/// defers like a capacity refusal and spends no budget.
pub const ENVIRONMENT_PROVISIONING_CODE: &str = "environment_provisioning";
/// Stable `RunError` code of a Permanent environment failure: an identical
/// retry against the same provider cannot succeed.
pub const ENVIRONMENT_REJECTED_CODE: &str = "environment_rejected";

/// Stable `RunError` code of a Rebuild whose closed Environment source has
/// nothing to rebuild from (no live checkpoint, retained volume or
/// Repository). It is a state conflict (HTTP 409) that no retry can resolve.
pub const SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE: &str =
    "session_environment_rebuild_source_unavailable";

/// The closed set of permanent state-conflict codes. An ordinary `Conflict`
/// is a lost compare-and-swap race that a fresh read may win, so it retries;
/// a code listed here names a conflict with committed state that no retry can
/// change, so the Worker fails it once instead of spending its budget.
const PERMANENT_CONFLICT_CODES: &[&str] = &[SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE];

/// Whether `code` names a permanent state conflict rather than a lost race.
#[must_use]
pub fn is_permanent_conflict_code(code: &str) -> bool {
    PERMANENT_CONFLICT_CODES.contains(&code)
}

/// The Worker class of an Environment the provider still reports
/// provisioning. Waiting is no failure until the provider's deadline passes:
/// before it the request defers (`NotReady`, no budget); from it on, or with
/// no deadline because no owner progresses the wait, it is a failed attempt
/// (`Retryable` within the caller's budget). It is never terminal: a later
/// attempt may find the Environment ready, or re-provision it.
#[must_use]
pub fn provisioning_disposition(
    deadline: Option<&awaken_provisioning_contract::ProvisioningDeadline>,
) -> SessionRealizationControlDisposition {
    match deadline {
        Some(deadline) if deadline.is_pending() => SessionRealizationControlDisposition::NotReady,
        _ => SessionRealizationControlDisposition::Retryable,
    }
}

/// Whether `code` names a request-local "not yet" rather than a failure: a
/// capacity refusal, a provision within its deadline, or Control not ready.
#[must_use]
pub fn is_not_ready_code(code: &str) -> bool {
    matches!(
        code,
        ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE
            | ENVIRONMENT_PROVISIONING_CODE
            | super::control_failure::SESSION_REALIZATION_NOT_READY_CODE
    )
}

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
        Disposition::Permanent | Disposition::StopRouting => {
            RunError::classified(ENVIRONMENT_REJECTED_CODE, message)
        }
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
/// fault and stable code, never its message: a capacity refusal or a provision
/// within its deadline is `NotReady` (the request-local deferral that keeps
/// every retry budget); any other Unavailable fault, a state conflict and an
/// internal fault no producer classified are `Retryable` within the caller's
/// retry budget; a request fault, a classified internal fault and a permanent
/// state conflict ([`is_permanent_conflict_code`]) are `Terminal`. A not-ready
/// code on a non-Unavailable fault is no deferral: it keeps its fault's class.
#[must_use]
pub fn effect_failure_disposition(error: &RunError) -> SessionRealizationControlDisposition {
    use awaken_session_types::RunErrorKind;
    match error.kind {
        RunErrorKind::Unavailable if is_not_ready_code(&error.code) => {
            SessionRealizationControlDisposition::NotReady
        }
        // An ordinary Conflict lost a race and may win on a fresh read; a
        // permanent conflict code conflicts with committed state for good.
        RunErrorKind::Conflict if is_permanent_conflict_code(&error.code) => {
            SessionRealizationControlDisposition::Terminal
        }
        RunErrorKind::Unavailable | RunErrorKind::Conflict => {
            SessionRealizationControlDisposition::Retryable
        }
        RunErrorKind::Internal if error.is_unclassified_internal() => {
            SessionRealizationControlDisposition::Retryable
        }
        RunErrorKind::Internal | RunErrorKind::BadRequest => {
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

/// A not-ready report (a capacity refusal or a provision within its deadline)
/// never consumes budget and so never exhausts it; a not-ready flag on a
/// non-retryable report is malformed and fails closed.
const fn realization_failure_outcome(
    retryable: bool,
    not_ready: bool,
    execution: SessionExecutionState,
    attempts: u32,
    budget: u32,
) -> Result<RealizationFailureOutcome, &'static str> {
    if not_ready && !retryable {
        return Err("a not-ready failure must be retryable");
    }
    // Only an assignment in Preparing or Activating counted an attempt.
    let assignment_counted = matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Activating
    );
    Ok(if not_ready {
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
    /// request-local "not yet", from [`effect_failure_disposition`].
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
            not_ready: disposition == SessionRealizationControlDisposition::NotReady,
            source_run_id,
            reason: error.to_string(),
        }
    }

    /// Classify the current root without mutating its compatible DTO. The
    /// controlled aggregate applies any counted-attempt refund separately.
    ///
    /// P1: callers can inspect the immutable plan. Persisted mutation remains
    /// with the aggregate's existing checked assignment operation and root CAS.
    ///
    /// ```
    /// use awaken_session_realization_contract::{
    ///     FailSessionRealization, RealizationFailureOutcome, SessionRealizationControlFailure,
    /// };
    /// use awaken_session_contract::PersistedSession;
    /// fn observe(command: &FailSessionRealization, session: &PersistedSession)
    ///     -> Result<RealizationFailureOutcome, SessionRealizationControlFailure>
    /// {
    ///     command.initial_realization_failure_plan(session, 3)
    /// }
    /// ```
    ///
    /// P2: the former mutable DTO shortcut is unavailable. This paired negative
    /// case uses the same valid public types as P1, so import errors cannot
    /// masquerade as closure. Other snapshot replacement paths remain outside
    /// this bounded removal; field encapsulation is checked separately.
    ///
    /// ```compile_fail,E0599
    /// use awaken_session_realization_contract::FailSessionRealization;
    /// use awaken_session_contract::PersistedSession;
    /// fn bypass(command: &FailSessionRealization, session: &mut PersistedSession) {
    ///     let _ = command.retries_initial_realization(session, 3);
    /// }
    /// ```
    pub fn initial_realization_failure_plan(
        &self,
        session: &PersistedSession,
        budget: u32,
    ) -> Result<RealizationFailureOutcome, crate::SessionRealizationControlFailure> {
        let outcome = realization_failure_outcome(
            self.retryable,
            self.not_ready,
            session.execution,
            spent_realization_budget(session),
            budget,
        )
        .map_err(|reason| crate::SessionRealizationControlFailure::Invalid(reason.into()))?;
        if session.execution == SessionExecutionState::Idle {
            return Ok(RealizationFailureOutcome::Absorb);
        }
        Ok(outcome)
    }
}

/// The budget an assignment has spent: counted claims for an initial
/// (Preparing/Activating) realization, otherwise the consecutive failed
/// effects of uncounted recovery or Run-free assignments, which the caller
/// records before planning. Recovery claims never count, so without this a
/// recovery whose effect always fails would be re-claimed forever.
fn spent_realization_budget(session: &PersistedSession) -> u32 {
    let progress = session.realization_progress();
    spent_budget(
        session.execution,
        progress.attempts(),
        progress.uncounted_failures(),
    )
}

const fn spent_budget(
    execution: SessionExecutionState,
    attempts: u32,
    uncounted_failures: u32,
) -> u32 {
    if matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Activating
    ) {
        attempts
    } else {
        uncounted_failures
    }
}

const fn run_free_exhausted(
    retryable: bool,
    not_ready: bool,
    uncounted_failures: u32,
    budget: u32,
) -> bool {
    !retryable || (!not_ready && uncounted_failures >= budget)
}

impl FailSessionRealization {
    /// Apply this failure's bounded-retry bookkeeping to the Session before
    /// the budget is planned: count it when its assignment counted no initial
    /// attempt (Run-free preparation, Running/Rescheduling recovery), and, for
    /// an Idle Session whose Run-free preparation is exhausted, fail exactly
    /// the Active generations whose publication was never acknowledged. Those
    /// generations are the demand that re-selects the Session on every poll;
    /// acknowledged (serving) generations are retained unchanged.
    pub fn record_bounded_failure(
        &self,
        session: &mut awaken_session_contract::SessionAggregate,
        budget: u32,
    ) -> Result<(), crate::SessionRealizationControlFailure> {
        let unavailable = |error: &dyn std::fmt::Display| {
            crate::SessionRealizationControlFailure::Unavailable(error.to_string())
        };
        session
            .assignment()
            .note_uncounted_failure(self.not_ready)
            .map_err(|error| unavailable(&error))?;
        if session.execution == SessionExecutionState::Idle
            && self.run_free_preparation_exhausted(session, budget)
        {
            session
                .mcp()
                .fail_unpublished_generations(self.reason.clone())
                .map_err(|error| unavailable(&error))?;
        }
        Ok(())
    }

    /// Whether an Idle Session's Run-free preparation may not be retried
    /// again: a terminal effect failure, or a retryable one that has spent the
    /// realization budget. A not-ready deferral never exhausts it. The caller
    /// then fails the unacknowledged MCP generations that keep demanding it.
    ///
    /// | retryable | not-ready | uncounted failures | exhausted |
    /// |-----------|-----------|--------------------|-----------|
    /// | false     | -         | any                | true      |
    /// | true      | true      | any                | false     |
    /// | true      | false     | < budget           | false     |
    /// | true      | false     | >= budget          | true      |
    #[must_use]
    pub fn run_free_preparation_exhausted(&self, session: &PersistedSession, budget: u32) -> bool {
        run_free_exhausted(
            self.retryable,
            self.not_ready,
            session.realization_progress().uncounted_failures(),
            budget,
        )
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
fn uncounted_failures_bound_recovery_and_run_free_preparation() {
    // For every execution, count and budget: an uncounted assignment spends
    // its consecutive failures, so a retryable non-deferred failure at or
    // above the budget always absorbs, and a deferral never exhausts.
    let execution = symbolic_execution();
    let attempts: u32 = kani::any();
    let failures: u32 = kani::any();
    let budget: u32 = kani::any();
    let spent = spent_budget(execution, attempts, failures);
    let counted = matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Activating
    );
    assert!(spent == if counted { attempts } else { failures });
    if !counted && failures >= budget {
        assert!(
            realization_failure_outcome(true, false, execution, spent, budget)
                == Ok(RealizationFailureOutcome::Absorb)
        );
        assert!(run_free_exhausted(true, false, failures, budget));
    }
    assert!(!run_free_exhausted(true, true, failures, budget));
    assert!(run_free_exhausted(false, false, failures, budget));
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

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
#[kani::proof]
fn provisioning_defers_only_before_the_providers_deadline() {
    use awaken_provisioning_contract::ProvisioningDeadline;
    // The u16 instant domain keeps the expiry law's millisecond-to-instant
    // conversion tractable, as the realization lease harnesses do; the
    // comparison it decides does not depend on magnitude.
    let deadline_ms = u64::from(kani::any::<u16>());
    let observed_ms = u64::from(kani::any::<u16>());
    let known: bool = kani::any();
    let reading = ProvisioningDeadline::new(deadline_ms, observed_ms);
    match provisioning_disposition(known.then_some(&reading)) {
        SessionRealizationControlDisposition::NotReady => {
            // A deferral needs a known deadline still ahead of the reading.
            assert!(known && observed_ms < deadline_ms);
        }
        SessionRealizationControlDisposition::Retryable => {
            assert!(!known || observed_ms >= deadline_ms);
        }
        SessionRealizationControlDisposition::Terminal => {
            panic!("a provisioning wait is never terminal")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncounted_recovery_and_run_free_failures_spend_the_same_budget() {
        // Causes: C1 execution (counted Preparing/Activating vs uncounted
        // Idle/Running/Rescheduling); C2 retryable; C3 not-ready deferral;
        // C4 spent count against a budget of 3.
        // S-rows (spent budget): S1 counted executions read the counted
        // attempts; S2 every other execution reads the consecutive uncounted
        // failures, so a recovery claim that never counts is still bounded.
        // O-rows (Running recovery outcome through the existing budget law):
        // O1 retryable below budget -> Retry; O2 retryable at budget -> Absorb;
        // O3 not-ready at any count -> Retry (no consumption); O4 terminal ->
        // Absorb. X-rows (Idle Run-free exhaustion): X1 terminal -> exhausted;
        // X2 not-ready -> never exhausted; X3 retryable below budget -> not;
        // X4 retryable at or above budget -> exhausted.
        use SessionExecutionState::{
            Activating, ActivationFailed, Idle, Preparing, Rescheduling, Running, Terminated,
        };
        for execution in [Preparing, Activating] {
            assert_eq!(spent_budget(execution, 2, 9), 2, "S1 {execution:?}");
        }
        for execution in [Idle, Running, Rescheduling, ActivationFailed, Terminated] {
            assert_eq!(spent_budget(execution, 2, 9), 9, "S2 {execution:?}");
        }
        let running = |retryable, not_ready, failures| {
            realization_failure_outcome(
                retryable,
                not_ready,
                Running,
                spent_budget(Running, 1, failures),
                3,
            )
            .unwrap()
        };
        let retry = RealizationFailureOutcome::Retry {
            refund_attempt: false,
        };
        assert_eq!(running(true, false, 1), retry, "O1");
        assert_eq!(running(true, false, 2), retry, "O1");
        assert_eq!(
            running(true, false, 3),
            RealizationFailureOutcome::Absorb,
            "O2"
        );
        assert_eq!(running(true, true, u32::MAX), retry, "O3");
        assert_eq!(
            running(false, false, 0),
            RealizationFailureOutcome::Absorb,
            "O4"
        );
        for failures in [0, 1, 3, u32::MAX] {
            assert!(
                run_free_exhausted(false, false, failures, 3),
                "X1 {failures}"
            );
            assert!(
                !run_free_exhausted(true, true, failures, 3),
                "X2 {failures}"
            );
        }
        assert!(!run_free_exhausted(true, false, 2, 3), "X3");
        assert!(run_free_exhausted(true, false, 3, 3), "X4");
        assert!(run_free_exhausted(true, false, 4, 3), "X4");
    }

    #[test]
    fn effect_failures_classify_by_fault_and_code_and_report_capacity() {
        // Causes: C1 the effect's fault (Unavailable / Conflict / Internal /
        // BadRequest); C2 its code is the capacity code; C3 an Internal code is
        // the unclassified one. Effects: E1 the Worker disposition; E2 the
        // FailSessionRealization (retryable, not_ready).
        // | Rule | C1 | C2 | E1 | E2 |
        // | F1 | Unavailable | yes (capacity or provisioning) | NotReady | (true, true) |
        // | F2 | Unavailable | no | Retryable | (true, false) |
        // | F4 | Conflict / unclassified Internal (C3) | no | Retryable | (true, false) |
        // | F3 | classified Internal/BadRequest | any | Terminal | (false, false) |
        // | F5 | Conflict with a permanent conflict code | no | Terminal | (false, false) |
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
                "F1",
                RunError::unavailable_classified(ENVIRONMENT_PROVISIONING_CODE, "provisioning"),
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
                "F4",
                RunError::conflict("raced"),
                SessionRealizationControlDisposition::Retryable,
                (true, false),
            ),
            (
                "F4",
                RunError::internal("unknown"),
                SessionRealizationControlDisposition::Retryable,
                (true, false),
            ),
            (
                "F5",
                RunError::conflict_classified(
                    SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE,
                    "closed source, nothing to rebuild from",
                ),
                SessionRealizationControlDisposition::Terminal,
                (false, false),
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
            assert_eq!((command.retryable, command.not_ready), report, "{rule}");
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
    fn a_provisioning_wait_defers_only_before_the_providers_deadline() {
        // Decision table: C1 the provider gave a deadline; C2 the observation
        // is before / at / after it. Effect: the Worker class.
        // | Rule | C1 | C2 | class |
        // | V1 | no (no owner progresses the wait, or a legacy observation) | - | Retryable |
        // | V2 | yes | before | NotReady (no budget) |
        // | V3 | yes | at or after | Retryable (spends the budget) |
        // Never Terminal: a later attempt may find it ready or re-provision.
        use awaken_provisioning_contract::ProvisioningDeadline;
        for (rule, deadline, class) in [
            ("V1", None, SessionRealizationControlDisposition::Retryable),
            (
                "V2",
                Some(ProvisioningDeadline::new(10_000, 9_999)),
                SessionRealizationControlDisposition::NotReady,
            ),
            (
                "V3",
                Some(ProvisioningDeadline::new(10_000, 10_000)),
                SessionRealizationControlDisposition::Retryable,
            ),
            (
                "V3",
                Some(ProvisioningDeadline::new(10_000, 60_000)),
                SessionRealizationControlDisposition::Retryable,
            ),
        ] {
            assert_eq!(provisioning_disposition(deadline.as_ref()), class, "{rule}");
        }
    }

    #[test]
    fn effect_failure_disposition_is_exhaustive_over_fault_and_code() {
        // Exhaustive decision table over the classification's whole input
        // domain that matters: every RunErrorKind x {each stable environment
        // code, the unclassified internal code, a foreign code}. Oracle derived
        // from the row, not the table: only Unavailable + capacity code defers;
        // any other Unavailable, any Conflict and an Internal fault carrying the
        // unclassified code retry; BadRequest and a classified Internal fault
        // are terminal whatever the code; a Conflict carrying a permanent
        // conflict code is terminal (no retry changes committed state).
        use awaken_session_types::RunErrorKind;
        use awaken_session_types::error::UNCLASSIFIED_INTERNAL_CODE;
        for kind in [
            RunErrorKind::Unavailable,
            RunErrorKind::Internal,
            RunErrorKind::BadRequest,
            RunErrorKind::Conflict,
        ] {
            for code in [
                ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                ENVIRONMENT_PROVISIONING_CODE,
                super::super::control_failure::SESSION_REALIZATION_NOT_READY_CODE,
                ENVIRONMENT_UNAVAILABLE_CODE,
                ENVIRONMENT_REJECTED_CODE,
                UNCLASSIFIED_INTERNAL_CODE,
                SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE,
                "unavailable",
                "conflict",
                "",
            ] {
                let error = RunError {
                    message: "m".into(),
                    kind,
                    code: code.into(),
                };
                let capacity = code == ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE
                    || code == ENVIRONMENT_PROVISIONING_CODE
                    || code == super::super::control_failure::SESSION_REALIZATION_NOT_READY_CODE;
                let unclassified = code == UNCLASSIFIED_INTERNAL_CODE;
                let permanent_conflict = kind == RunErrorKind::Conflict
                    && code == SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE;
                let expected = match (kind, capacity, unclassified) {
                    _ if permanent_conflict => SessionRealizationControlDisposition::Terminal,
                    (RunErrorKind::Unavailable, true, _) => {
                        SessionRealizationControlDisposition::NotReady
                    }
                    (RunErrorKind::Unavailable | RunErrorKind::Conflict, _, _)
                    | (RunErrorKind::Internal, _, true) => {
                        SessionRealizationControlDisposition::Retryable
                    }
                    (RunErrorKind::Internal | RunErrorKind::BadRequest, _, _) => {
                        SessionRealizationControlDisposition::Terminal
                    }
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
    fn conflict_and_unclassified_internal_retry_until_the_realization_budget_is_spent() {
        // Decision table: C1 the failed effect's fault (state conflict /
        // unclassified internal / classified internal / request fault);
        // C2 the Session's counted attempts against the realization retry
        // budget. Effect: the aggregate's outcome for the reported failure.
        // | Rule | C1 | C2 | outcome |
        // | E1 | conflict / unclassified internal | remaining | Retry, no refund |
        // | E2 | conflict / unclassified internal | exhausted | Absorb |
        // | E3 | classified internal / request fault | remaining | Absorb |
        // The report is built from the error exactly as the driver builds it.
        let budget = 3;
        let lease = crate::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/1".into(),
            epoch: 1,
            expires_at_unix_ms: 1,
        };
        let retry = Ok(RealizationFailureOutcome::Retry {
            refund_attempt: false,
        });
        let absorb = Ok(RealizationFailureOutcome::Absorb);
        let rows = [
            ("E1", RunError::conflict("raced"), 2, retry),
            ("E1", RunError::internal("unknown"), 0, retry),
            ("E2", RunError::conflict("raced"), 3, absorb),
            ("E2", RunError::internal("unknown"), 3, absorb),
            (
                "E3",
                RunError::classified("session_realization_invariant_violated", "broken"),
                0,
                absorb,
            ),
            ("E3", RunError::bad_request("bad"), 0, absorb),
        ];
        for (rule, error, attempts, expected) in rows {
            let report =
                FailSessionRealization::for_effect("s".into(), lease.clone(), None, None, &error);
            assert!(!report.not_ready, "{rule}");
            for execution in [
                SessionExecutionState::Preparing,
                SessionExecutionState::Running,
            ] {
                assert_eq!(
                    realization_failure_outcome(
                        report.retryable,
                        report.not_ready,
                        execution,
                        attempts,
                        budget,
                    ),
                    expected,
                    "{rule} {execution:?}"
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
        let malformed = Err("a not-ready failure must be retryable");
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
    #[test]
    fn effect_error_payload_keeps_classification_and_only_shortens_its_hint() {
        // Cause/effect table E1-E4. C1 neutral error kind/code spans the
        // existing NotReady/Retryable/Terminal partitions; C2 hint is absent,
        // zero or positive; C3 reporting wait is below/equal/above the hint.
        // E1 !C2 => None; E2 Some(h)+wait<h => Some(h-wait);
        // E3 Some(h)+wait>=h => Some(0), never discarded; E4 every C1/C2/C3
        // keeps the exact kind/code/message and original classification.
        // The Kani harness exhausts duration arithmetic; these rows prove the
        // real error payload and its neutral projection compose with it.
        use std::time::Duration as D;
        let errors: [fn() -> RunError; 8] = [
            || RunError::unavailable_classified(ENVIRONMENT_PROVISIONING_CODE, "original"),
            || RunError::unavailable_classified(ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, "original"),
            || {
                RunError::unavailable_classified(
                    super::super::control_failure::SESSION_REALIZATION_NOT_READY_CODE,
                    "original",
                )
            },
            || RunError::unavailable("original"),
            || RunError::conflict("original"),
            || RunError::internal("original"),
            || RunError::classified("classified", "original"),
            || RunError::bad_request("original"),
        ];
        for error in errors {
            for hint in [None, Some(D::ZERO), Some(D::from_secs(5)), Some(D::MAX)] {
                for wait in [
                    D::ZERO,
                    D::from_secs(2),
                    D::from_secs(5),
                    D::from_secs(8),
                    D::MAX,
                ] {
                    let original = error();
                    let expected = (
                        original.kind,
                        original.code.clone(),
                        original.message.clone(),
                        effect_failure_disposition(&original),
                    );
                    let failure =
                        SessionRealizationEffectFailure::new(original, hint).after_wait(wait);
                    assert_eq!(
                        failure.retry_after(),
                        hint.map(|h| h.saturating_sub(wait)),
                        "E1-E3"
                    );
                    assert_eq!(
                        effect_failure_disposition(failure.run_error()),
                        expected.3,
                        "E4 class"
                    );
                    let projected =
                        RunError::from(super::super::SessionRealizationDriveError::Effect(failure));
                    assert_eq!(
                        (projected.kind, projected.code, projected.message),
                        (expected.0, expected.1, expected.2),
                        "E4 exact error"
                    );
                }
            }
        }
        assert!(
            SessionRealizationEffectFailure::from(RunError::internal("plain"))
                .retry_after()
                .is_none(),
            "E1 neutral failure has no invented hint"
        );
    }
}
