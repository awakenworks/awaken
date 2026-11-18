// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_event_batch.run_reservation_precedes_distinct_activity_receipt
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
//! Durable activity fencing for overlapping Session Runs.

use crate::SessionAggregate;
use crate::mutation::ROOT_CAS_ATTEMPTS;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::runs::RunError;
#[cfg(any(test, kani))]
use awaken_session_contract::SessionActivitySettlementPlan;
#[cfg(any(test, kani))]
use awaken_session_contract::SessionExecutionState;
#[cfg(any(test, kani))]
use awaken_session_contract::session_activity_open_plan;
#[cfg(test)]
use awaken_session_contract::session_activity_settlement_plan;
use awaken_session_contract::{IdempotencyRecord, PersistedSession};
use awaken_session_types::SessionRevision;
use awaken_time::WallClockExt as _;

use std::sync::Arc;

use awaken_session_execution_contract::ThreadReads;

use super::{SessionMutationError, mutation::repository_failure};

/// Failure from a Session activity transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionActivityError {
    #[error(transparent)]
    Admission(crate::mutation::SessionAdmissionFailure),
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal and cannot begin another activity")]
    Terminal,
    #[error("Session is not ready for activity")]
    NotReady,
    #[error("Session list-cost budget has been reached")]
    BudgetReached,
    #[error("Session activity epoch is exhausted")]
    EpochExhausted,
    #[error("Session revision conflict")]
    Conflict,
    /// The Session root advanced past the revision whose Runtime projection
    /// this fresh activity was admitted against; nothing was applied. Only the
    /// admission that recovered that projection can recover it again.
    #[error("Session root advanced past the recovered Runtime projection")]
    ProjectionSuperseded,
    #[error("Session activity persistence is unavailable: {0}")]
    Unavailable(String),
    /// Persistence refused before applying the activity; nothing changed.
    #[error("Session activity was not applied: {0}")]
    NotApplied(String),
    #[error("Session activity durable state is invalid: {0}")]
    Internal(String),
    /// The aggregate or budget ledger refused the activity transition: the
    /// same permanent class as `Internal`, carrying its typed source.
    #[error("Session activity durable state is invalid: {0}")]
    Invalid(crate::SessionStateViolation),
}

use awaken_session_contract::SessionActivityAdmissionPlan as SessionActivityOpenPlan;

/// A fresh activity may consume only the root revision whose complete Runtime
/// projection was recovered immediately before reservation. Exact activity
/// receipt replay is checked first and remains independent recovery truth.
const fn activity_projection_revision_matches(
    current_revision: u64,
    projected_revision: Option<u64>,
) -> bool {
    match projected_revision {
        None => true,
        Some(projected) => projected == current_revision,
    }
}

#[cfg(kani)]
/// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
/// @obligation session_event_batch.run_reservation_precedes_distinct_activity_receipt
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
#[kani::proof]
fn session_activity_open_and_settlement_plans_are_total_exact_and_fail_closed() {
    // Admission causes: all seven execution states, arbitrary current/exact
    // epoch, external-realization permission, committed-Awaiting continuation,
    // and budget admission. Effects: terminal/not-ready/budget/overflow/conflict
    // reject; Open always carries one non-zero epoch greater than the fence.
    // Settlement crosses the real Option<bool> observation with every execution
    // state through the production conversion. Only the final Running member
    // closes; absence stutters. SQL CAS and interval persistence remain outside.
    let execution = match kani::any::<u8>() % 7 {
        0 => SessionExecutionState::Preparing,
        1 => SessionExecutionState::Activating,
        2 => SessionExecutionState::ActivationFailed,
        3 => SessionExecutionState::Running,
        4 => SessionExecutionState::Rescheduling,
        5 => SessionExecutionState::Idle,
        _ => SessionExecutionState::Terminated,
    };
    let current_epoch = kani::any();
    let exact_epoch = kani::any::<bool>().then_some(kani::any());
    let allow_external = kani::any();
    let continue_await = kani::any();
    let budget_admits = kani::any();
    let open = session_activity_open_plan(
        execution,
        current_epoch,
        exact_epoch,
        allow_external,
        continue_await,
        budget_admits,
    );
    let ready = execution.admits_activity()
        || (allow_external
            && matches!(
                execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            ));
    let policy_admits = !execution.is_terminal() && ready && (continue_await || budget_admits);
    assert_eq!(
        open == SessionActivityOpenPlan::Terminal,
        execution.is_terminal()
    );
    assert_eq!(
        open == SessionActivityOpenPlan::NotReady,
        !execution.is_terminal() && !ready
    );
    assert_eq!(
        open == SessionActivityOpenPlan::BudgetReached,
        !execution.is_terminal() && ready && !continue_await && !budget_admits
    );
    assert_eq!(
        open == SessionActivityOpenPlan::Conflict,
        policy_admits && exact_epoch.is_some_and(|epoch| epoch == 0 || epoch <= current_epoch)
    );
    assert_eq!(
        open == SessionActivityOpenPlan::EpochExhausted,
        policy_admits && exact_epoch.is_none() && current_epoch == u64::MAX
    );
    assert_eq!(
        matches!(open, SessionActivityOpenPlan::Open(_)),
        policy_admits
            && (exact_epoch.is_some_and(|epoch| epoch != 0 && epoch > current_epoch)
                || (exact_epoch.is_none() && current_epoch != u64::MAX))
    );
    if let SessionActivityOpenPlan::Open(epoch) = open {
        assert_ne!(epoch, 0);
        assert!(epoch > current_epoch);
        assert!(!execution.is_terminal());
        assert!(
            execution.admits_activity()
                || (allow_external
                    && matches!(
                        execution,
                        SessionExecutionState::Preparing | SessionExecutionState::Activating
                    ))
        );
        assert!(continue_await || budget_admits);
    }
    let current_revision: u64 = kani::any();
    let projected_revision = kani::any::<bool>().then_some(kani::any());
    assert_eq!(
        activity_projection_revision_matches(current_revision, projected_revision),
        projected_revision.is_none_or(|projected| projected == current_revision)
    );
    let observation: Option<bool> = kani::any();
    let expected = match observation {
        None => SessionActivitySettlementPlan::Stutter,
        Some(false) => SessionActivitySettlementPlan::RetainExecution,
        Some(true) => match execution {
            SessionExecutionState::Running => SessionActivitySettlementPlan::CloseRunningInterval,
            _ => SessionActivitySettlementPlan::RetainExecution,
        },
    };
    assert_eq!(
        SessionActivitySettlementPlan::from_epoch_settlement(observation, execution),
        expected
    );
}

#[cfg(test)]
#[test]
fn settlement_observation_covers_every_execution_state() {
    // C1: no exact removal, another member remains, or the final member settles.
    // C2: each of the seven execution states. E1: absence stutters; E2: a
    // surviving member retains execution; E3: only final+Running closes the
    // interval. The 3x7 table exercises the same conversion as the async path.
    use SessionActivitySettlementPlan::{CloseRunningInterval, RetainExecution, Stutter};
    use SessionExecutionState::{
        Activating, ActivationFailed, Idle, Preparing, Rescheduling, Running, Terminated,
    };

    for (execution, final_effect) in [
        (Preparing, RetainExecution),
        (Activating, RetainExecution),
        (ActivationFailed, RetainExecution),
        (Running, CloseRunningInterval),
        (Rescheduling, RetainExecution),
        (Idle, RetainExecution),
        (Terminated, RetainExecution),
    ] {
        for (observation, expected) in [
            (None, Stutter),
            (Some(false), RetainExecution),
            (Some(true), final_effect),
        ] {
            assert_eq!(
                SessionActivitySettlementPlan::from_epoch_settlement(observation, execution),
                expected,
                "observation={observation:?}, execution={execution:?}"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn session_activity_plan_cause_effect_table_is_closed() {
    // R1 Idle+budget+fresh -> next epoch; R2 exact greater epoch -> that epoch;
    // R3 terminal, unready, budget-denied, stale exact and MAX each reject;
    // S1 exact final Running removal closes; S2 another active retains; S3
    // duplicate/unknown stutters. Kani supplies arbitrary scalar coverage.
    assert_eq!(
        session_activity_open_plan(SessionExecutionState::Idle, 4, None, false, false, true),
        SessionActivityOpenPlan::Open(5)
    );
    assert_eq!(
        session_activity_open_plan(
            SessionExecutionState::Running,
            4,
            Some(9),
            false,
            true,
            false,
        ),
        SessionActivityOpenPlan::Open(9)
    );
    for rejected in [
        session_activity_open_plan(
            SessionExecutionState::Terminated,
            0,
            None,
            false,
            false,
            true,
        ),
        session_activity_open_plan(
            SessionExecutionState::Rescheduling,
            0,
            None,
            false,
            false,
            true,
        ),
        session_activity_open_plan(SessionExecutionState::Idle, 0, None, false, false, false),
        session_activity_open_plan(
            SessionExecutionState::Running,
            4,
            Some(4),
            false,
            true,
            true,
        ),
        session_activity_open_plan(
            SessionExecutionState::Running,
            u64::MAX,
            None,
            false,
            true,
            true,
        ),
    ] {
        assert!(!matches!(rejected, SessionActivityOpenPlan::Open(_)));
    }
    assert_eq!(
        session_activity_settlement_plan(true, true, true),
        SessionActivitySettlementPlan::CloseRunningInterval
    );
    assert_eq!(
        session_activity_settlement_plan(true, false, true),
        SessionActivitySettlementPlan::RetainExecution
    );
    assert_eq!(
        session_activity_settlement_plan(false, true, true),
        SessionActivitySettlementPlan::Stutter
    );
}

impl SessionActivityError {
    pub(super) fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::Admission(error) => Self::Admission(error),
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::Internal(
                "activity mutation idempotency key unexpectedly changed payload".into(),
            ),
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
            SessionMutationError::NotApplied(message) => Self::NotApplied(message),
            SessionMutationError::Internal(message) => Self::Internal(message),
            SessionMutationError::Invalid(violation) => Self::Invalid(violation),
        }
    }

    pub(crate) fn run_error(self) -> RunError {
        match self {
            Self::Admission(error) => error.into(),
            Self::NotFound => RunError::bad_request("Session was not found"),
            Self::Terminal => RunError::bad_request("Session no longer accepts new messages"),
            Self::NotReady => RunError::unavailable_classified(
                "session_not_ready",
                "Session realization has not completed",
            ),
            Self::BudgetReached => RunError::classified(
                "budget_reached",
                "Session list-cost budget has been reached",
            ),
            Self::EpochExhausted => RunError::classified(
                "session_activity_epoch_exhausted",
                "Session activity epoch is exhausted",
            ),
            Self::Conflict | Self::ProjectionSuperseded => {
                RunError::unavailable("Session activity changed concurrently")
            }
            Self::Unavailable(message) | Self::NotApplied(message) => {
                RunError::unavailable(message)
            }
            Self::Internal(message) => SessionMutationError::Internal(message).into(),
            Self::Invalid(violation) => SessionMutationError::Invalid(violation).into(),
        }
    }
}

struct ActivityProjection {
    revision: Option<SessionRevision>,
    admitted_after_cursor: Option<u64>,
}

/// Durable activity fencing for overlapping Session Runs: open, transfer and
/// recover one activity epoch per Run operation through the root CAS, after
/// the Environment is Resident. It holds the CAS kernel, the clock, the Thread
/// read role whose commit cursor admits a runtime interval, and the
/// continuation saga that makes the Environment Resident first.
pub struct SessionActivities {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    thread_reads: Arc<dyn ThreadReads>,
    continuations: Arc<crate::SessionContinuations>,
    #[cfg(any(test, feature = "test-support"))]
    runtime_incarnation: String,
}

impl SessionActivities {
    pub(crate) fn new(
        mutations: Arc<crate::SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
        runtime: &crate::SessionExecutionRoles,
        continuations: Arc<crate::SessionContinuations>,
        #[cfg_attr(not(any(test, feature = "test-support")), allow(unused_variables))]
        runtime_incarnation: &str,
    ) -> Self {
        Self {
            mutations,
            clock,
            thread_reads: runtime.thread_reads.clone(),
            continuations,
            #[cfg(any(test, feature = "test-support"))]
            runtime_incarnation: runtime_incarnation.to_owned(),
        }
    }

    fn is_run_activity_operation(operation_id: &str) -> bool {
        operation_id.starts_with("session-run-activity:")
    }

    /// The original accepted-activity receipt identity. Composition reuses it
    /// for an exact final receipt observation, never another key derivation.
    pub fn activity_operation_record(session_id: &str, operation_id: &str) -> IdempotencyRecord {
        let identity = awaken_agent_contract::stable_fingerprint(&(
            "managed-session-activity-v1",
            session_id,
            operation_id,
        ));
        IdempotencyRecord {
            key: format!("managed:session-activity:{session_id}:{identity}"),
            payload_hash: identity,
        }
    }

    async fn activity_operation_receipt(
        &self,
        session_id: &awaken_session_types::SessionId,
        record: &IdempotencyRecord,
    ) -> Result<Option<u64>, SessionActivityError> {
        let receipt = self
            .mutations
            .repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        match receipt {
            Some(receipt) if receipt.payload_hash != record.payload_hash => {
                Err(SessionActivityError::Internal(
                    "Session activity operation id was reused with another payload".into(),
                ))
            }
            Some(receipt) => Ok(Some(receipt.committed_revision.0)),
            None => Ok(None),
        }
    }

    async fn replayed_activity_operation_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        record: &IdempotencyRecord,
        operation: &str,
        source: Option<(&ThreadId, &RunId)>,
        snapshot: impl std::future::Future<Output = Result<PersistedSession, SessionActivityError>>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        let Some(epoch) = self.activity_operation_receipt(session_id, record).await? else {
            return Ok(None);
        };
        // Receipt absence/hash mismatch must not poll either snapshot source.
        // The caller selects its original fresh read or already-reloaded value.
        let session = snapshot.await?;
        session
            .validate_run_activity_replay(operation, epoch, source)
            .map_err(|_| SessionActivityError::Conflict)?;
        Self::validate_observed_activity_root(commit, &session).await?;
        Ok(Some((session, epoch)))
    }

    async fn replay_activity_from_repository_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        record: &IdempotencyRecord,
        operation_id: &str,
        source: Option<(&ThreadId, &RunId)>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        self.replayed_activity_operation_using(
            session_id,
            record,
            operation_id,
            source,
            async {
                self.mutations
                    .session_aggregate(session_id)
                    .await
                    .map(SessionAggregate::into_snapshot)
                    .map_err(repository_failure)
                    .map_err(SessionActivityError::mutation)
            },
            commit,
        )
        .await
    }

    async fn validate_observed_activity_root(
        commit: &dyn crate::SessionRootCommit,
        session: &PersistedSession,
    ) -> Result<(), SessionActivityError> {
        if commit
            .validate(&session.session_id, session.revision)
            .await
            .map_err(SessionActivityError::mutation)?
        {
            Ok(())
        } else {
            Err(SessionActivityError::Conflict)
        }
    }

    pub(crate) fn open_activity_on_snapshot(
        session: &mut SessionAggregate,
        exact_epoch: Option<u64>,
        allow_external_realization: bool,
        continue_committed_await: bool,
        now_ms: u64,
        admitted_after_commit_cursor: u64,
        source: Option<(&str, &ThreadId, &RunId)>,
    ) -> Result<u64, SessionActivityError> {
        let epoch = match session.activity().open(
            exact_epoch,
            allow_external_realization,
            continue_committed_await,
            now_ms,
            admitted_after_commit_cursor,
            source,
        ) {
            SessionActivityOpenPlan::Open(epoch) => epoch,
            SessionActivityOpenPlan::Terminal => return Err(SessionActivityError::Terminal),
            SessionActivityOpenPlan::NotReady => return Err(SessionActivityError::NotReady),
            SessionActivityOpenPlan::BudgetReached => {
                return Err(SessionActivityError::BudgetReached);
            }
            SessionActivityOpenPlan::EpochExhausted => {
                return Err(SessionActivityError::EpochExhausted);
            }
            SessionActivityOpenPlan::Conflict => return Err(SessionActivityError::Conflict),
        };
        Ok(epoch)
    }

    /// The root Thread's Runtime commit cursor before an admission: every
    /// lifecycle commit of the interval that admission opens follows it. Read
    /// only when the admission may open an interval, since a joining activity
    /// keeps the running interval's own cursor.
    async fn interval_admission_cursor(
        &self,
        session: &PersistedSession,
    ) -> Result<u64, SessionActivityError> {
        let thread_reads = self.thread_reads.as_ref();

        if session.running_interval.is_some() {
            return Ok(0);
        }
        thread_reads
            .session_thread_recovery_snapshot(&session.session_id, session.session_id.as_str())
            .await
            .map(|snapshot| snapshot.map_or(0, |snapshot| snapshot.store_cursor))
            .map_err(|error| SessionActivityError::Unavailable(error.to_string()))
    }

    /// Admit one driving event and return the committed aggregate carrying its
    /// monotonically increasing completion fence.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn begin_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionActivityError> {
        let mutations = &self.mutations;
        match mutations.session_aggregate(session_id).await {
            Ok(session) if session.is_terminal() => return Err(SessionActivityError::Terminal),
            Ok(_) => {}
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return Err(SessionActivityError::NotFound);
            }
            Err(error) => return Err(SessionActivityError::mutation(repository_failure(error))),
        }
        self.ensure_activity_environment_resident(session_id)
            .await?;
        let admission = mutations.fresh_mutation_record(
            self.runtime_incarnation.as_str(),
            session_id.as_str(),
            "begin-activity",
        );
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            // A driving event is also the trigger that makes a registered Worker
            // claim and realize a freshly prepared Session. Preserve that
            // stronger realization phase until it converges: replacing it with
            // `running` would let a failed Stage look like an ordinary Run and
            // a later settlement could erase `activation_failed` back to idle.
            let external = session.requires_external_environment_owner();
            let admitted_after = self.interval_admission_cursor(&session).await?;
            Self::open_activity_on_snapshot(
                &mut session,
                None,
                external,
                false,
                self.clock.now_ms(),
                admitted_after,
                None,
            )?;
            match mutations
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    admission.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Open the durable activity for one stable coordination operation. The
    /// existing Session mutation receipt is the sole operation→epoch mapping:
    /// its committed root revision is the epoch, so exact retries (including a
    /// retry after settlement) return the same value without reopening activity.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn begin_activity_for_operation(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            false,
            None,
            None,
            None,
        )
        .await
    }

    /// Open a fresh Run activity only while the Session root still matches the
    /// complete execution projection recovered before Runtime reservation.
    ///
    /// | Refusal | Durable effect | Caller answer |
    /// |---|---|---|
    /// | budget reached | rejection receipt | `budget_reached`, replayed by every later recovery |
    /// | proven write rollback | this attempt did not commit | `session_run_not_admitted` |
    /// | unknown write, observation outage, CAS conflict or superseded projection | original identity stays repairable | `session_run_admission_pending` |
    /// | other | none | its own class |
    ///
    /// The refusal stays typed; [`Self::activity_operation_error`] gives the
    /// caller answer, so the Run admission that recovered `projected_revision`
    /// can tell a superseded projection from every other refusal.
    pub(crate) async fn begin_projected_run_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        projected_revision: SessionRevision,
        source: (&ThreadId, &RunId),
    ) -> Result<u64, SessionActivityError> {
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            false,
            None,
            Some(projected_revision),
            Some(source),
        )
        .await
        .map(|(_, epoch)| epoch)
    }

    /// The reservation is durable but its activity is not: the outcome is not
    /// yet decided, and the same operation is the only safe retry.
    fn admission_pending(error: SessionActivityError) -> RunError {
        RunError::unavailable_classified(
            "session_run_admission_pending",
            format!(
                "Session Run admission is pending; retry the same operation: {}",
                error.run_error().message
            ),
        )
    }

    /// The store proved it applied nothing: the Run was not admitted. Like a
    /// pending outcome, only the same operation may retry it, because the
    /// durable reservation can still be completed by repair.
    fn admission_not_applied(error: SessionActivityError) -> RunError {
        RunError::unavailable_classified(
            "session_run_not_admitted",
            format!(
                "Session Run was not admitted; retry the same operation: {}",
                error.run_error().message
            ),
        )
    }

    /// Run reservations remain undecided on failed observations or CAS, and
    /// are not admitted when the store proved it applied nothing; other
    /// coordination operations keep their original activity error classes.
    pub(crate) fn activity_operation_error(
        operation_id: &str,
        error: SessionActivityError,
    ) -> RunError {
        match error {
            error @ SessionActivityError::NotApplied(_)
                if Self::is_run_activity_operation(operation_id) =>
            {
                Self::admission_not_applied(error)
            }
            error @ (SessionActivityError::Unavailable(_)
            | SessionActivityError::Conflict
            | SessionActivityError::ProjectionSuperseded)
                if Self::is_run_activity_operation(operation_id) =>
            {
                Self::admission_pending(error)
            }
            error => error.run_error(),
        }
    }

    /// The original durable budget-refusal receipt identity, shared with the
    /// final application gate without copying its fingerprint or key formula.
    pub fn budget_rejection_record(session_id: &str, operation_id: &str) -> IdempotencyRecord {
        let identity = awaken_agent_contract::stable_fingerprint(&(
            "managed-session-run-budget-rejection-v1",
            session_id,
            operation_id,
        ));
        IdempotencyRecord {
            key: format!("managed:session-run-rejection:{session_id}:{identity}"),
            payload_hash: identity,
        }
    }

    /// Resolve an obsolete budget decision against current receipts and the
    /// latest root. Both fresh Run admission and reservation repair reach this
    /// owner; a returned BudgetReached is backed by a durable refusal receipt.
    /// The gated SQLite/native table covers this I/O/CAS race; existing scalar
    /// Kani and TLA results do not prove mutable-budget/negative-receipt ordering.
    async fn resolve_run_budget_rejection_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        continue_committed_await: bool,
        admitted_after: u64,
        source: Option<(&ThreadId, &RunId)>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        let owner = self
            .mutations
            .owner(session_id)
            .await
            .map_err(SessionActivityError::mutation)?;
        let mut session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        // Receipts outrank mutable policy, including an accepted epoch which
        // has already settled. Read after this snapshot so a preceding winner
        // cannot be overwritten by a rejection compiled from its newer root.
        if let Some(committed) = self
            .recover_activity_for_operation_using(session_id, operation_id, source, commit)
            .await?
        {
            return Ok(committed);
        }
        let epoch = session
            .revision
            .0
            .checked_add(1)
            .ok_or(SessionActivityError::EpochExhausted)?;
        let external = session.requires_external_environment_owner();
        match Self::open_activity_on_snapshot(
            &mut session,
            Some(epoch),
            external,
            continue_committed_await,
            self.clock.now_ms(),
            admitted_after,
            source.map(|(thread, run)| (operation_id, thread, run)),
        ) {
            // The aggregate rejects before mutation. Only this unchanged
            // latest candidate may carry a fresh refusal through its root CAS.
            Err(SessionActivityError::BudgetReached) => {}
            // A raise without an accepted receipt is undecided here. Discard
            // the locally opened candidate: its old execution projection and
            // cursor cannot authorize a fresh activity on the newer root.
            Ok(_) => return Err(SessionActivityError::Conflict),
            Err(error) => return Err(error),
        }
        let record = Self::budget_rejection_record(session_id.as_str(), operation_id);
        // Applied and replayed refusals both certify the same durable negative fact.
        let committed_rejection = self
            .mutations
            .commit_session_snapshot_with_record_using(commit, &owner, session, record, Vec::new())
            .await
            .map_err(SessionActivityError::mutation)?;
        Self::validate_observed_activity_root(commit, &committed_rejection.into_session()).await?;
        Err(SessionActivityError::BudgetReached)
    }

    /// Read only the exact committed activity receipt for crash recovery. This
    /// never evaluates fresh admission and never opens an epoch; cancellation
    /// uses it to distinguish a never-admitted reservation from one whose
    /// already-committed activity still requires canonical settlement.
    pub(crate) async fn recover_activity_for_operation(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        source: Option<(&ThreadId, &RunId)>,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        self.recover_activity_for_operation_using(
            session_id,
            operation_id,
            source,
            self.mutations.as_ref(),
        )
        .await
    }

    pub(crate) async fn recover_activity_for_operation_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        source: Option<(&ThreadId, &RunId)>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        if operation_id.trim().is_empty() {
            return Err(SessionActivityError::Unavailable(
                "Session activity operation identity is required".into(),
            ));
        }
        let record = Self::activity_operation_record(session_id.as_str(), operation_id);
        let replayed = self
            .replay_activity_from_repository_using(
                session_id,
                &record,
                operation_id,
                source,
                commit,
            )
            .await?;
        // A Run activity refused at the budget boundary keeps that answer for
        // its caller's retry, ordinary repair and cancellation alike.
        let rejected = replayed.is_none()
            && Self::is_run_activity_operation(operation_id)
            && self
                .activity_operation_receipt(
                    session_id,
                    &Self::budget_rejection_record(session_id.as_str(), operation_id),
                )
                .await?
                .is_some();
        if replayed.is_none() {
            // No accepted receipt has polled a Root snapshot. Preserve that
            // original missing-root/None observation; final claim validation
            // is not evidence that a Root exists or a new epoch is authorized.
            match commit.validate_claim().await {
                Ok(()) => {}
                Err(SessionMutationError::Conflict) => {
                    // The final gate observed a same-operation receipt after
                    // the cold absence. Read that ORIGINAL receipt once through
                    // the same deferred Root/source law. Continued absence is
                    // uncertainty, never a normal rejection or a new epoch.
                    return self
                        .replay_activity_from_repository_using(
                            session_id,
                            &record,
                            operation_id,
                            source,
                            commit,
                        )
                        .await?
                        .map(Some)
                        .ok_or(SessionActivityError::Conflict);
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        if rejected {
            return Err(SessionActivityError::BudgetReached);
        }
        Ok(replayed)
    }

    /// Atomically replace an already-coordinated dispatch activity with the
    /// activity for its exact committed continuation. The old Worker may settle
    /// before or after this CAS: removing an already-settled epoch is a no-op,
    /// while a later stale settlement cannot close the newly opened interval.
    pub(crate) async fn transfer_committed_activity_for_operation(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        prior_activity_epoch: Option<u64>,
        source: (&ThreadId, &RunId),
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        if prior_activity_epoch == Some(0) {
            return Err(SessionActivityError::Unavailable(
                "prior Session activity epoch must be nonzero".into(),
            ));
        }
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            true,
            prior_activity_epoch,
            None,
            Some(source),
        )
        .await
    }

    /// Use the original continuation owner and one activity error mapping.
    async fn ensure_activity_environment_resident(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionActivityError> {
        self.continuations
            .ensure_environment_resident(session_id, self.clock.now_ms())
            .await
            .map_err(|error| match error {
                super::SessionContinuationError::Terminal => SessionActivityError::Terminal,
                error => SessionActivityError::Unavailable(error.to_string()),
            })
    }

    /// After receipt replay, check the projection before physical recovery.
    /// Only the original continuation may advance an ineligible Environment's
    /// revision; a fresh activity is still fenced again inside the root CAS.
    async fn activity_revision_after_continuation(
        &self,
        session_id: &awaken_session_types::SessionId,
        projected_revision: Option<SessionRevision>,
    ) -> Result<Option<SessionRevision>, SessionActivityError> {
        let continuation_may_advance = match projected_revision {
            Some(projected_revision) => {
                let projected_root = self
                    .mutations
                    .session_aggregate(session_id)
                    .await
                    .map_err(repository_failure)
                    .map_err(SessionActivityError::mutation)?;
                if !activity_projection_revision_matches(
                    projected_root.revision.0,
                    Some(projected_revision.0),
                ) {
                    return Err(SessionActivityError::ProjectionSuperseded);
                }
                !projected_root.environment.admits_realization_effects()
            }
            None => false,
        };
        let resident = self
            .ensure_activity_environment_resident(session_id)
            .await?;
        Ok(projected_revision.map(|revision| {
            if continuation_may_advance {
                resident.revision
            } else {
                revision
            }
        }))
    }

    pub(crate) async fn begin_activity_for_operation_with_policy(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        continue_committed_await: bool,
        prior_activity_epoch: Option<u64>,
        projected_revision: Option<SessionRevision>,
        source: Option<(&ThreadId, &RunId)>,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        self.begin_activity_for_operation_with_policy_using(
            session_id,
            operation_id,
            continue_committed_await,
            prior_activity_epoch,
            projected_revision,
            source,
            self.mutations.as_ref(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn begin_activity_for_operation_with_policy_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        continue_committed_await: bool,
        prior_activity_epoch: Option<u64>,
        projected_revision: Option<SessionRevision>,
        source: Option<(&ThreadId, &RunId)>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        if let Some(committed) = self
            .recover_activity_for_operation_using(session_id, operation_id, source, commit)
            .await?
        {
            return Ok(committed);
        }

        let projected_revision = self
            .activity_revision_after_continuation(session_id, projected_revision)
            .await?;
        self.commit_activity_for_operation_using(
            session_id,
            operation_id,
            continue_committed_await,
            prior_activity_epoch,
            ActivityProjection {
                revision: projected_revision,
                admitted_after_cursor: None,
            },
            source,
            commit,
        )
        .await
    }

    /// Prepare physical/continuation effects before a Dispatch admission guard
    /// is acquired. The guarded callback may use only the shared receipt/CAS
    /// loop below and a cursor from its authoritative Thread snapshot.
    pub(crate) async fn prepare_run_activity_revision(
        &self,
        session_id: &awaken_session_types::SessionId,
        projected_revision: SessionRevision,
    ) -> Result<SessionRevision, SessionActivityError> {
        self.prepare_run_activity_revision_using(
            session_id,
            projected_revision,
            self.mutations.as_ref(),
        )
        .await
    }

    pub(crate) async fn prepare_run_activity_revision_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        projected_revision: SessionRevision,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<SessionRevision, SessionActivityError> {
        // Preparation may converge the original Session-owned continuation,
        // under that owner's grant and Root CAS, with no Dispatch transaction.
        // It grants no claim-specific epoch; final activity CAS uses the gate.
        let revision = self
            .activity_revision_after_continuation(session_id, Some(projected_revision))
            .await?
            .ok_or(SessionActivityError::Conflict)?;
        if !commit
            .validate(session_id, revision)
            .await
            .map_err(SessionActivityError::mutation)?
        {
            return Err(SessionActivityError::Conflict);
        }
        Ok(revision)
    }

    pub(crate) async fn commit_prepared_run_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        projected_revision: SessionRevision,
        source: (&ThreadId, &RunId),
        admitted_after_cursor: Option<u64>,
        publication_condition: Option<awaken_session_types::SessionPublicationCondition>,
    ) -> Result<u64, RunError> {
        let conditional = publication_condition.as_ref().map(|condition| {
            crate::mutation::ConditionalSessionRootCommit {
                mutations: self.mutations.as_ref(),
                session: session_id,
                condition,
            }
        });
        let commit: &dyn crate::SessionRootCommit = match &conditional {
            Some(commit) => commit,
            None => self.mutations.as_ref(),
        };
        self.commit_prepared_run_activity_using(
            session_id,
            operation_id,
            projected_revision,
            source,
            admitted_after_cursor,
            commit,
        )
        .await
        .map_err(|error| Self::activity_operation_error(operation_id, error))
    }

    pub(crate) async fn commit_prepared_run_activity_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        projected_revision: SessionRevision,
        source: (&ThreadId, &RunId),
        admitted_after_cursor: Option<u64>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<u64, SessionActivityError> {
        self.commit_activity_for_operation_using(
            session_id,
            operation_id,
            false,
            None,
            ActivityProjection {
                revision: Some(projected_revision),
                admitted_after_cursor,
            },
            Some(source),
            commit,
        )
        .await
        .map(|(_, epoch)| epoch)
    }

    /// The sole accepted/refused receipt and root-CAS loop. No projection,
    /// continuation, Dispatch operation or synchronous publication runs here.
    #[allow(clippy::too_many_arguments)]
    async fn commit_activity_for_operation_using(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        continue_committed_await: bool,
        prior_activity_epoch: Option<u64>,
        projection: ActivityProjection,
        source: Option<(&ThreadId, &RunId)>,
        commit: &dyn crate::SessionRootCommit,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        let ActivityProjection {
            revision: projected_revision,
            admitted_after_cursor,
        } = projection;
        let mutations = &self.mutations;
        let record = Self::activity_operation_record(session_id.as_str(), operation_id);
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            if let Some(committed) = self
                .recover_activity_for_operation_using(session_id, operation_id, source, commit)
                .await?
            {
                return Ok(committed);
            }
            let owner_scope = mutations
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            // Close the receipt/read race: an exact accepted or refused Run
            // receipt committed before this snapshot wins before we compile an
            // epoch. A cap raise cannot erase a preceding durable refusal.
            if let Some(committed) = self
                .recover_activity_for_operation_using(session_id, operation_id, source, commit)
                .await?
            {
                return Ok(committed);
            }
            if !activity_projection_revision_matches(
                session.revision.0,
                projected_revision.map(|revision| revision.0),
            ) {
                return Err(SessionActivityError::ProjectionSuperseded);
            }
            let epoch = session
                .revision
                .0
                .checked_add(1)
                .ok_or(SessionActivityError::EpochExhausted)?;
            let external = session.requires_external_environment_owner();
            let admitted_after = match admitted_after_cursor {
                Some(cursor) => cursor,
                None => self.interval_admission_cursor(&session).await?,
            };
            if let Err(error) = Self::open_activity_on_snapshot(
                &mut session,
                Some(epoch),
                external,
                continue_committed_await,
                self.clock.now_ms(),
                admitted_after,
                source.map(|(thread, run)| (operation_id, thread, run)),
            ) {
                return if error == SessionActivityError::BudgetReached
                    && Self::is_run_activity_operation(operation_id)
                {
                    drop(session);
                    self.resolve_run_budget_rejection_using(
                        session_id,
                        operation_id,
                        continue_committed_await,
                        admitted_after,
                        source,
                        commit,
                    )
                    .await
                } else {
                    Err(error)
                };
            }
            if let Some(prior_activity_epoch) = prior_activity_epoch {
                // The new epoch was inserted first, so removing the old one can
                // never make the aggregate falsely Idle inside this mutation.
                let _ = session
                    .activity()
                    .settle_activity_epoch(prior_activity_epoch);
            }
            match mutations
                .commit_session_snapshot_with_record_using(
                    commit,
                    &owner_scope,
                    session,
                    record.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok(snapshot_commit) if snapshot_commit.transitioned() => {
                    let session = snapshot_commit.into_session();
                    if session.revision.0 != epoch {
                        return Err(SessionActivityError::Unavailable(
                            "Session activity revision did not match its committed epoch".into(),
                        ));
                    }
                    return Ok((session, epoch));
                }
                Ok(snapshot_commit) => {
                    let snapshot = std::future::ready(Ok(snapshot_commit.into_session()));
                    return self
                        .replayed_activity_operation_using(
                            session_id,
                            &record,
                            operation_id,
                            source,
                            snapshot,
                            commit,
                        )
                        .await?
                        .ok_or_else(|| {
                            SessionActivityError::Unavailable(
                                "replayed Session activity has no idempotency receipt".into(),
                            )
                        });
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }
}

#[cfg(test)]
mod final_admission_tests {
    use super::*;
    use awaken_session_contract::{SessionRepository as _, SessionRepositoryReads as _};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReceiptAtFinalGate {
        activities: Arc<SessionActivities>,
        session: awaken_session_types::SessionId,
        operation: String,
        thread: ThreadId,
        run: RunId,
        publish: bool,
        claim_checks: AtomicUsize,
        root_checks: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::SessionRootCommit for ReceiptAtFinalGate {
        async fn commit(
            &self,
            _: &str,
            _: awaken_session_contract::SessionMutation,
        ) -> Result<awaken_session_contract::SessionMutationResult, SessionMutationError> {
            panic!("receipt recovery must not call a second activity mutation");
        }

        async fn validate_claim(&self) -> Result<(), SessionMutationError> {
            self.claim_checks.fetch_add(1, Ordering::SeqCst);
            if self.publish {
                self.activities
                    .begin_activity_for_operation_with_policy(
                        &self.session,
                        &self.operation,
                        false,
                        None,
                        None,
                        Some((&self.thread, &self.run)),
                    )
                    .await
                    .expect("the original independent contender commits its activity");
            }
            Err(SessionMutationError::Conflict)
        }

        async fn validate(
            &self,
            session: &awaken_session_types::SessionId,
            observed_revision: SessionRevision,
        ) -> Result<bool, SessionMutationError> {
            self.root_checks.fetch_add(1, Ordering::SeqCst);
            let current = self
                .activities
                .mutations
                .repository()
                .get(session)
                .await
                .map_err(repository_failure)?;
            Ok(session == self.session && current.revision == observed_revision)
        }
    }

    #[tokio::test]
    async fn same_operation_receipt_is_reread_once_after_final_gate_conflict() {
        // Causes: H9 cold accepted receipt absent; the final gate then returns
        // Conflict with that same receipt now present or still absent. Effects:
        // present => original epoch/source law from a fresh Root, no second
        // mutation; absent => Conflict, never None/Rejected. Exactly one gate
        // call owns the cut, so a hidden retry loop cannot pass this test.
        // The existing SQLite application fixture supplies the real receipt
        // and Root CAS. This tests recovery ordering, not actual PG claim locks.
        for publish in [false, true] {
            let store = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            let session =
                awaken_session_types::SessionId::from(format!("activity-receipt-cut-{publish}"));
            crate::tests::create(
                store.as_ref(),
                crate::tests::persisted(session.as_str(), false, "idle"),
            )
            .await;
            let app = crate::tests::application_fixture::application(
                store.clone(),
                Arc::new(crate::tests::RecordingEnvironmentSource::default()),
            );
            let thread = ThreadId(session.to_string());
            let run = RunId("same-operation".into());
            let operation = awaken_session_execution_contract::session_run_activity_operation_id(
                &session, &run,
            );
            let before = store.get(&session).await.unwrap();
            let gate = ReceiptAtFinalGate {
                activities: app.activities.clone(),
                session: session.clone(),
                operation: operation.clone(),
                thread: thread.clone(),
                run: run.clone(),
                publish,
                claim_checks: AtomicUsize::new(0),
                root_checks: AtomicUsize::new(0),
            };
            let result = app
                .activities
                .recover_activity_for_operation_using(
                    &session,
                    &operation,
                    Some((&thread, &run)),
                    &gate,
                )
                .await;
            assert_eq!(
                gate.claim_checks.load(Ordering::SeqCst),
                1,
                "H9 one final cut"
            );
            if publish {
                let (root, epoch) = result.unwrap().expect("H9 exact accepted receipt recovery");
                let record =
                    SessionActivities::activity_operation_record(session.as_str(), &operation);
                let receipt = store
                    .idempotency_receipt(&session, &record.key)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt.payload_hash, record.payload_hash);
                assert_eq!(
                    receipt.committed_revision.0, epoch,
                    "H9 original receipt is sole epoch owner"
                );
                assert_eq!(root, store.get(&session).await.unwrap());
                assert!(root.revision.0 > before.revision.0);
                assert_eq!(
                    gate.root_checks.load(Ordering::SeqCst),
                    1,
                    "H9 reread Root validated"
                );
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    SessionActivityError::Conflict,
                    "H9 continued absence stays uncertain"
                );
                assert_eq!(store.get(&session).await.unwrap(), before);
                assert_eq!(
                    gate.root_checks.load(Ordering::SeqCst),
                    0,
                    "H9 no receipt means no snapshot read"
                );
            }
            let missing = awaken_session_types::SessionId::from("missing-root-no-receipt");
            assert!(
                app.activities
                    .recover_activity_for_operation(&missing, "missing-operation", None)
                    .await
                    .unwrap()
                    .is_none(),
                "ordinary missing Root retains original None"
            );
        }
    }
}
