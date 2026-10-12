// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_event_batch.run_reservation_precedes_distinct_activity_receipt
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
//! Durable activity fencing for overlapping Session Runs.

use crate::SessionAggregate;

use awaken_agent_contract::RunLifecycleCursor;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_event_contract::SessionEventProjectionAnchor;
#[cfg(any(test, kani))]
use awaken_session_state_contract::session_activity_open_plan;
use awaken_session_state_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    SessionEventDispatchSettlementEffect, SessionExecutionState, SessionRuntimeInterval,
    SessionRuntimeIntervalObservation,
};
use awaken_session_types::RunError;
use awaken_session_types::SessionRevision;
use awaken_time::WallClockExt as _;

use super::{SessionApplication, SessionMutationError, mutation::repository_failure};

pub(crate) fn runtime_interval_fact(
    owner_scope: &str,
    session_id: &str,
    interval: SessionRuntimeInterval,
) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: interval.interval_id.clone(),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.runtime_interval_closed".to_string(),
        timestamp: i64::try_from(interval.ended_at_unix_ms / 1_000).unwrap_or(i64::MAX),
        runtime_interval: Some(interval),
    }
}

/// Failure from a Session activity transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionActivityError {
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
    #[error("Session activity persistence is unavailable: {0}")]
    Unavailable(String),
}

use awaken_session_state_contract::SessionActivityAdmissionPlan as SessionActivityOpenPlan;

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

/// The logical effects after settling one exact activity epoch.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionActivitySettlementPlan {
    Stutter,
    RetainExecution,
    CloseRunningInterval,
}

/// Decide the logical effect after the aggregate's exact epoch set has applied
/// a removal. Only removal of the final current epoch may close Running.
const fn session_activity_settlement_plan(
    exact_epoch_removed: bool,
    last_active: bool,
    execution_running: bool,
) -> SessionActivitySettlementPlan {
    if !exact_epoch_removed {
        SessionActivitySettlementPlan::Stutter
    } else if last_active && execution_running {
        SessionActivitySettlementPlan::CloseRunningInterval
    } else {
        SessionActivitySettlementPlan::RetainExecution
    }
}

#[cfg(kani)]
/// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
/// @obligation session_event_batch.run_reservation_precedes_distinct_activity_receipt
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
/// @proof-input-debt free_symbol session_activity_settlement_plan exact_epoch_removed
/// @proof-input-debt free_symbol session_activity_settlement_plan execution_running
/// @proof-input-debt free_symbol session_activity_settlement_plan last_active
#[kani::proof]
fn session_activity_open_and_settlement_plans_are_total_exact_and_fail_closed() {
    // Admission causes: all seven execution states, arbitrary current/exact
    // epoch, external-realization permission, committed-Awaiting continuation,
    // and budget admission. Effects: terminal/not-ready/budget/overflow/conflict
    // reject; Open always carries one non-zero epoch greater than the fence.
    // Settlement causes cross exact removal, last-active, and Running; only the
    // exact+last+Running row closes the interval, while unknown/duplicate is a
    // stutter. SQL CAS and interval persistence remain outside this plan.
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
    let removed = kani::any();
    let last = kani::any();
    let running = kani::any();
    let settlement = session_activity_settlement_plan(removed, last, running);
    assert_eq!(
        settlement == SessionActivitySettlementPlan::CloseRunningInterval,
        removed && last && running
    );
    assert_eq!(
        settlement == SessionActivitySettlementPlan::Stutter,
        !removed
    );
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
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::Unavailable(
                "activity mutation idempotency key unexpectedly changed payload".into(),
            ),
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }

    pub(crate) fn run_error(self) -> RunError {
        match self {
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
            Self::EpochExhausted => RunError::internal("Session activity epoch is exhausted"),
            Self::Conflict => RunError::unavailable("Session activity changed concurrently"),
            Self::Unavailable(message) => RunError::unavailable(message),
        }
    }
}

impl SessionApplication {
    fn activity_operation_record(session_id: &str, operation_id: &str) -> IdempotencyRecord {
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
            .session_repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        match receipt {
            Some(receipt) if receipt.payload_hash != record.payload_hash => {
                Err(SessionActivityError::Unavailable(
                    "Session activity operation id was reused with another payload".into(),
                ))
            }
            Some(receipt) => Ok(Some(receipt.committed_revision.0)),
            None => Ok(None),
        }
    }

    async fn replayed_activity_operation(
        &self,
        session_id: &awaken_session_types::SessionId,
        record: &IdempotencyRecord,
        operation: &str,
        source: Option<(&ThreadId, &RunId)>,
    ) -> Result<Option<(PersistedSession, u64)>, SessionActivityError> {
        let Some(epoch) = self.activity_operation_receipt(session_id, record).await? else {
            return Ok(None);
        };
        let session = self
            .session_aggregate(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionActivityError::mutation)?;
        session
            .validate_run_activity_replay(operation, epoch, source)
            .map_err(|_| SessionActivityError::Conflict)?;
        Ok(Some((session.into_snapshot(), epoch)))
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
        let thread_reads = self.runtime().thread_reads();

        if session.running_interval.is_some() {
            return Ok(0);
        }
        thread_reads
            .session_thread_recovery_snapshot(&session.session_id, session.session_id.as_str())
            .await
            .map(|snapshot| snapshot.map_or(0, |snapshot| snapshot.store_cursor))
            .map_err(|error| SessionActivityError::Unavailable(error.to_string()))
    }

    /// Recover/admit the frozen Session binding, then open exactly one activity
    /// for a stable protocol operation. The existing operation receipt is the
    /// only retry coordinate; the protocol receives the committed epoch and
    /// must pass it through instead of performing another admission check.
    pub async fn begin_admitted_activity_for_operation(
        &self,
        agent_id: &str,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        source: (&ThreadId, &RunId),
    ) -> Result<(PersistedSession, u64), RunError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::internal(
                "Session activity operation identity is required",
            ));
        }
        let record = Self::activity_operation_record(session_id.as_str(), operation_id);
        // Exact committed receipts are recovery truth. Re-read them before
        // fresh policy so a response-loss repair can recover the original epoch
        // even if the Session subsequently reached a budget or terminal state.
        if let Some(committed) = self
            .replayed_activity_operation(session_id, &record, operation_id, Some(source))
            .await
            .map_err(SessionActivityError::run_error)?
        {
            return Ok(committed);
        }
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionActivityError::mutation)
            .map_err(SessionActivityError::run_error)?;
        self.admit_run_session(&owner_scope, session_id.as_str(), agent_id)
            .await?;
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            false,
            None,
            None,
            Some(source),
        )
        .await
        .map_err(SessionActivityError::run_error)
    }

    /// Admit one driving event and return the committed aggregate carrying its
    /// monotonically increasing completion fence.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn begin_activity(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionActivityError> {
        match self.session_aggregate(session_id).await {
            Ok(session) if session.is_terminal() => return Err(SessionActivityError::Terminal),
            Ok(_) => {}
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => {
                return Err(SessionActivityError::NotFound);
            }
            Err(error) => return Err(SessionActivityError::Unavailable(error.to_string())),
        }
        self.ensure_environment_resident(session_id, self.clock.now_ms())
            .await
            .map_err(|error| match error {
                super::SessionContinuationError::Terminal => SessionActivityError::Terminal,
                error => SessionActivityError::Unavailable(error.to_string()),
            })?;
        let admission = self.fresh_mutation_record(session_id.as_str(), "begin-activity");
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
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
            match self
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    admission.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
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

    /// Open a fresh activity only while the Session root still matches the
    /// complete execution projection recovered before Runtime reservation.
    pub(crate) async fn begin_activity_for_operation_at_revision(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        projected_revision: SessionRevision,
        source: (&ThreadId, &RunId),
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        self.begin_activity_for_operation_with_policy(
            session_id,
            operation_id,
            false,
            None,
            Some(projected_revision),
            Some(source),
        )
        .await
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
        if operation_id.trim().is_empty() {
            return Err(SessionActivityError::Unavailable(
                "Session activity operation identity is required".into(),
            ));
        }
        let record = Self::activity_operation_record(session_id.as_str(), operation_id);
        self.replayed_activity_operation(session_id, &record, operation_id, source)
            .await
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

    pub(crate) async fn begin_activity_for_operation_with_policy(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation_id: &str,
        continue_committed_await: bool,
        prior_activity_epoch: Option<u64>,
        projected_revision: Option<SessionRevision>,
        source: Option<(&ThreadId, &RunId)>,
    ) -> Result<(PersistedSession, u64), SessionActivityError> {
        if operation_id.trim().is_empty() {
            return Err(SessionActivityError::Unavailable(
                "Session activity operation identity is required".into(),
            ));
        }
        let record = Self::activity_operation_record(session_id.as_str(), operation_id);
        if let Some(committed) = self
            .replayed_activity_operation(session_id, &record, operation_id, source)
            .await?
        {
            return Ok(committed);
        }

        let continuation_may_advance = match projected_revision {
            Some(projected_revision) => {
                let projected_root = self
                    .session_aggregate(session_id)
                    .await
                    .map_err(repository_failure)
                    .map_err(SessionActivityError::mutation)?;
                if !activity_projection_revision_matches(
                    projected_root.revision.0,
                    Some(projected_revision.0),
                ) {
                    return Err(SessionActivityError::Conflict);
                }
                !crate::realization::environment_admits_realization_effects(
                    &projected_root.environment,
                )
            }
            None => false,
        };
        // Admission receipt above still wins first. The projected branch alone
        // reads and checks its pre-continuation revision; both paths then use
        // the same existing continuation operation and error law exactly once.
        let resident = self
            .ensure_environment_resident(session_id, self.clock.now_ms())
            .await
            .map_err(|error| match error {
                super::SessionContinuationError::Terminal => SessionActivityError::Terminal,
                error => SessionActivityError::Unavailable(error.to_string()),
            })?;
        let projected_revision = projected_revision.map(|revision| {
            if continuation_may_advance {
                resident.revision
            } else {
                revision
            }
        });
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if let Some(committed) = self
                .replayed_activity_operation(session_id, &record, operation_id, source)
                .await?
            {
                return Ok(committed);
            }
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            // Close the receipt/read race: if a concurrent exact caller
            // committed after our pre-loop check but before this snapshot, its
            // durable receipt wins before we compile another epoch.
            if let Some(committed) = self
                .replayed_activity_operation(session_id, &record, operation_id, source)
                .await?
            {
                return Ok(committed);
            }
            if !activity_projection_revision_matches(
                session.revision.0,
                projected_revision.map(|revision| revision.0),
            ) {
                return Err(SessionActivityError::Conflict);
            }
            let epoch = session
                .revision
                .0
                .checked_add(1)
                .ok_or(SessionActivityError::EpochExhausted)?;
            let external = session.requires_external_environment_owner();
            let admitted_after = self.interval_admission_cursor(&session).await?;
            Self::open_activity_on_snapshot(
                &mut session,
                Some(epoch),
                external,
                continue_committed_await,
                self.clock.now_ms(),
                admitted_after,
                source.map(|(thread, run)| (operation_id, thread, run)),
            )?;
            if let Some(prior_activity_epoch) = prior_activity_epoch {
                // The new epoch was inserted first, so removing the old one can
                // never make the aggregate falsely Idle inside this mutation.
                let _ = session
                    .activity()
                    .settle_activity_epoch(prior_activity_epoch);
            }
            match self
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    record.clone(),
                    Vec::new(),
                )
                .await
            {
                Ok(commit) if commit.transitioned() => {
                    let session = commit.into_session();
                    if session.revision.0 != epoch {
                        return Err(SessionActivityError::Unavailable(
                            "Session activity revision did not match its committed epoch".into(),
                        ));
                    }
                    return Ok((session, epoch));
                }
                Ok(commit) => {
                    let session = commit.into_session();
                    let epoch = self
                        .activity_operation_receipt(session_id, &record)
                        .await?
                        .ok_or_else(|| {
                            SessionActivityError::Unavailable(
                                "replayed Session activity has no idempotency receipt".into(),
                            )
                        })?;
                    session
                        .validate_run_activity_replay(operation_id, epoch, source)
                        .map_err(|_| SessionActivityError::Conflict)?;
                    return Ok((session, epoch));
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Settle one admitted driving event. Completion order is irrelevant: every
    /// active epoch is removed exactly once, and only the last removal closes
    /// the shared Running interval. Terminal truth fences every completion.
    pub async fn settle_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
    ) -> Result<PersistedSession, SessionActivityError> {
        self.settle_activity_observed(
            &awaken_session_types::SessionId::from(session_id),
            expected_epoch,
            None,
        )
        .await
    }

    /// Settle one admitted activity while retaining the exact Runtime commit
    /// boundary that caused it. Infrastructure-only and definitively rejected
    /// activities carry no observation; they still close through this same root
    /// CAS and never create a protocol-side lifecycle owner.
    pub async fn settle_activity_observed(
        &self,
        session_id: &awaken_session_types::SessionId,
        expected_epoch: u64,
        observation: Option<SessionRuntimeIntervalObservation>,
    ) -> Result<PersistedSession, SessionActivityError> {
        self.settle_activity_observed_for_event(session_id, expected_epoch, observation, None)
            .await
    }

    /// Consume an already-applied Dispatch settlement by joining its exact
    /// User Event marker and activity removal in the existing Session root CAS.
    pub(crate) async fn settle_user_event_dispatch_observed(
        &self,
        session_id: &awaken_session_types::SessionId,
        expected_epoch: u64,
        observation: SessionRuntimeIntervalObservation,
        run_id: &RunId,
        boundary: awaken_session_execution_contract::SessionAgentRunBoundary,
    ) -> Result<PersistedSession, SessionActivityError> {
        self.settle_activity_observed_for_event(
            session_id,
            expected_epoch,
            Some(observation),
            Some((run_id, boundary)),
        )
        .await
    }

    async fn settle_activity_observed_for_event(
        &self,
        session_id: &awaken_session_types::SessionId,
        expected_epoch: u64,
        observation: Option<SessionRuntimeIntervalObservation>,
        event_run_id: Option<(
            &RunId,
            awaken_session_execution_contract::SessionAgentRunBoundary,
        )>,
    ) -> Result<PersistedSession, SessionActivityError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            if session.is_terminal() {
                return Ok(session.into_snapshot());
            }
            if let Some(observation) = observation.clone() {
                if session.closed_runtime_intervals.iter().any(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                }) {
                    return Ok(session.into_snapshot());
                }
                // A reply may transfer the activity while its predecessor's
                // leased Worker is still closing. Fence that stale observation
                // before comparing it with the predecessor's already-closed
                // boundary: it belongs to neither the closed interval nor the
                // successor and is therefore an exact no-op.
                if !(session.active_activity_epochs.contains(&expected_epoch)
                    || (session.active_activity_epochs.is_empty()
                        && session.execution == SessionExecutionState::Running
                        && session.activity_epoch == expected_epoch))
                {
                    return Ok(session.into_snapshot());
                }
                if session.closed_runtime_intervals.iter().any(|interval| {
                    interval.observations.iter().any(|existing| {
                        existing.activity_epoch == expected_epoch && existing != &observation
                    })
                }) {
                    return Err(SessionActivityError::Unavailable(
                        "Session activity epoch was reused with another Runtime boundary".into(),
                    ));
                }
                let exact_replay = session.running_interval.as_ref().is_some_and(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                });
                let before_admission =
                    session.runtime_observation_precedes_interval_admission(&observation);
                if before_admission && !exact_replay && event_run_id.is_some() {
                    return Err(SessionActivityError::Unavailable(
                        "User Event Runtime boundary precedes its Session activity admission"
                            .into(),
                    ));
                }
                // An externally realized Session may admit and activate a Run
                // while its stronger Preparing/Activating phase is still
                // authoritative. If cancellation commits before realization,
                // no customer-visible Running interval ever opened; settle the
                // admitted epoch without fabricating one or attaching the Run
                // boundary to a nonexistent interval.
                let settles_before_runtime_interval = session.running_interval.is_none()
                    && matches!(
                        session.execution,
                        SessionExecutionState::Preparing | SessionExecutionState::Activating
                    );
                if !exact_replay
                    && !settles_before_runtime_interval
                    && !before_admission
                    && !session.activity().observe_runtime_interval(observation)
                {
                    return Err(SessionActivityError::Unavailable(
                        "Runtime boundary does not belong to the active Session interval".into(),
                    ));
                }
            }
            let settlement = match event_run_id {
                Some((run_id, boundary)) => {
                    let observation = observation.as_ref().ok_or_else(|| {
                        SessionActivityError::Unavailable(
                            "User Event Dispatch settlement has no commit observation".into(),
                        )
                    })?;
                    match session
                        .activity()
                        .settle_user_event_dispatch(
                            run_id,
                            boundary,
                            expected_epoch,
                            SessionEventProjectionAnchor {
                                source_commit_cursor: observation.source_commit_cursor,
                            },
                            self.clock.now_ms(),
                        )
                        .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?
                    {
                        SessionEventDispatchSettlementEffect::NotEvent
                        | SessionEventDispatchSettlementEffect::Replayed {
                            fully_settled: false,
                        } => return Ok(session.into_snapshot()),
                        SessionEventDispatchSettlementEffect::Replayed {
                            fully_settled: true,
                        } => Some(true),
                        SessionEventDispatchSettlementEffect::Applied { fully_settled } => {
                            Some(fully_settled)
                        }
                    }
                }
                None => session.activity().settle_activity_epoch(expected_epoch),
            };
            let settlement_plan = session_activity_settlement_plan(
                settlement.is_some(),
                settlement == Some(true),
                session.execution == SessionExecutionState::Running,
            );
            if settlement_plan == SessionActivitySettlementPlan::Stutter {
                return Ok(session.into_snapshot());
            }
            // Completion order does not grant inverse-transition ownership.
            // Every admitted epoch is removed exactly once; only the removal of
            // the final active epoch may close the one shared Running interval.
            // Initial Worker realization may still be preparing/activating, in
            // which case settlement records the activity completion without
            // overwriting that stronger execution phase.
            let lifecycle_facts =
                if settlement_plan == SessionActivitySettlementPlan::CloseRunningInterval {
                    session
                        .activity()
                        .finish_running(self.clock.now_ms())
                        .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?
                        .map(|interval| {
                            runtime_interval_fact(&owner_scope, session_id.as_str(), interval)
                        })
                        .into_iter()
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
            let emitted_runtime_interval = !lifecycle_facts.is_empty();
            match self
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    if event_run_id.is_some() {
                        "settle-event-dispatch"
                    } else {
                        "settle-activity"
                    },
                    lifecycle_facts,
                )
                .await
            {
                Ok(commit) => {
                    if emitted_runtime_interval {
                        self.notify_lifecycle_fact();
                    }
                    return Ok(commit.into_session());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Retain a child or intermediate Runtime boundary without settling the
    /// shared activity epoch. A child-to-primary report continuation uses this
    /// path so the final interval carries both exact commit coordinates.
    pub(crate) async fn observe_activity_runtime_boundary(
        &self,
        session_id: &awaken_session_types::SessionId,
        observation: SessionRuntimeIntervalObservation,
    ) -> Result<PersistedSession, SessionActivityError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionActivityError::mutation)?;
            let mut session = self
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionActivityError::mutation)?;
            if session.is_terminal()
                || session.closed_runtime_intervals.iter().any(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                })
                || session.running_interval.as_ref().is_some_and(|interval| {
                    interval
                        .observations
                        .iter()
                        .any(|existing| existing == &observation)
                })
            {
                return Ok(session.into_snapshot());
            }
            if session.closed_runtime_intervals.iter().any(|interval| {
                interval.observations.iter().any(|existing| {
                    existing.activity_epoch == observation.activity_epoch
                        && existing != &observation
                })
            }) {
                return Err(SessionActivityError::Unavailable(
                    "Session activity epoch was reused with another Runtime boundary".into(),
                ));
            }
            if !session
                .activity()
                .observe_runtime_interval(observation.clone())
            {
                return Err(SessionActivityError::Unavailable(
                    "Runtime boundary does not belong to the active Session interval".into(),
                ));
            }
            match self
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "observe-runtime-boundary",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Resolve an exact terminal coordinate from the same prefix used to settle
    /// this Run. The activity clock must not reopen a global lifecycle history.
    pub(crate) fn runtime_interval_observation(
        activity_epoch: u64,
        thread_id: &ThreadId,
        run_id: &RunId,
        state: &RunState,
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<Option<SessionRuntimeIntervalObservation>, RunError> {
        let mut selected = None;
        let page = snapshot
            .lifecycle_events_after(RunLifecycleCursor::default(), usize::MAX)
            .map_err(|error| RunError::internal(error.to_string()))?;
        for event in page.events {
            if &event.thread_id == thread_id
                && &event.run_id == run_id
                && &event.state == state
                && awaken_session_event_contract::SessionAgentRunBoundary::is_boundary(event.kind)
            {
                selected = Some(SessionRuntimeIntervalObservation {
                    activity_epoch,
                    thread_id: event.thread_id,
                    run_id: event.run_id,
                    lifecycle_cursor: event.cursor,
                    source_commit_cursor: event.source_commit_cursor,
                });
            }
        }
        Ok(selected)
    }
}
