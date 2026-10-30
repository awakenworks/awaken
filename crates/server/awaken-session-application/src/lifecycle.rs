// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal evidence session_budget.accepted_cost_arithmetic_never_wraps
// Existing settlement bodies move with their obligations; this is no new SQL, CAS or Notify proof.
// Budget settlement integrates the original pure contract kernel; this adds no
// production arithmetic entrypoint or heap/CAS proof. B10 owns the integration regression.
//! Durable Delete intent, cumulative budget facts and the existing lifecycle signals.
use crate::credentials::mutation_failure;
use crate::mutation::{ROOT_CAS_ATTEMPTS, repository_failure};
use crate::terminal::pending_event_batch_error;
use crate::{SessionActivityError, SessionAggregate};
use crate::{
    SessionDispositionMutation, SessionMutationError, SessionMutations, SessionPreparationError,
};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_contract::LifecycleOutbox;
use awaken_session_contract::SessionEventProjectionAnchor;
use awaken_session_contract::{ManagedBudgetUsageCursor, SessionUsage};
use awaken_session_contract::{
    ManagedLifecycleFact, PersistedSession, SessionActivitySettlementPlan,
    SessionEventDispatchSettlementEffect, SessionExecutionState, SessionRuntimeIntervalObservation,
};
use awaken_time::WallClockExt as _;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone)]
pub struct BudgetSettlementOutcome {
    pub session: PersistedSession,
    pub reached_now: bool,
}

/// Protocol-neutral request to commit the durable Session Delete fence.
/// Application code, not an edge adapter, derives the canonical lifecycle fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDeleteCommand {
    session_id: awaken_session_types::SessionId,
    /// A test's fixed request instant. Production commands carry none: the
    /// application stamps the request on its own clock when it admits it.
    requested_at_unix_s: Option<i64>,
}

impl SessionDeleteCommand {
    #[must_use]
    pub fn new(session_id: impl Into<awaken_session_types::SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            requested_at_unix_s: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn at_for_test(session_id: &str, requested_at_unix_s: i64) -> Self {
        Self {
            session_id: session_id.into(),
            requested_at_unix_s: Some(requested_at_unix_s),
        }
    }
}

/// Stateful lifecycle command owner. The original root CAS, Clock, wake and
/// outbox are composed once; no cleanup driver or delivery consumer lives here.
///
/// ```
/// use awaken_session_application::{SessionLifecycle, SessionDeleteCommand};
/// async fn delete_intent(service: &SessionLifecycle) {
///     let _ = service.commit_delete_intent(SessionDeleteCommand::new("session")).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::{SessionApplication, SessionDeleteCommand};
/// async fn late_app_command(app: &SessionApplication) {
///     let _ = app.commit_delete_intent(SessionDeleteCommand::new("session")).await;
/// }
/// ```
///
/// B9 capability pair: cumulative settlement uses this already constructed
/// lifecycle owner and the same root CAS/Clock; App has no duplicate command.
/// These source checks accompany the original real SQLite/coordination cases.
///
/// ```
/// use awaken_session_application::SessionLifecycle;
/// use awaken_session_contract::SessionUsage;
/// use awaken_session_types::SessionId;
/// async fn settle(service: &SessionLifecycle, id: &SessionId, usage: SessionUsage) {
///     let _ = service.reconcile_managed_budget_usage(id, usage).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// use awaken_session_contract::SessionUsage;
/// use awaken_session_types::SessionId;
/// async fn old_command(app: &SessionApplication, id: &SessionId, usage: SessionUsage) {
///     let _ = app.reconcile_managed_budget_usage(id, usage).await;
/// }
/// ```
///
/// Activity command source capabilities A1-A3: the existing Lifecycle owns
/// the observed command; the two old App entrances are independently absent.
/// These functions are not invoked and do not prove Runtime/SQL execution.
///
/// ```
/// use awaken_session_application::SessionLifecycle;
/// use awaken_session_types::SessionId;
/// async fn settle(service: &SessionLifecycle, id: &SessionId, epoch: u64) {
///     let _ = service.settle_activity_observed(id, epoch, None).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// async fn old_none_wrapper(app: &SessionApplication) {
///     let _ = app.settle_activity("session", 1).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// use awaken_session_types::SessionId;
/// async fn old_observed_command(app: &SessionApplication, id: &SessionId) {
///     let _ = app.settle_activity_observed(id, 1, None).await;
/// }
/// ```
pub struct SessionLifecycle {
    mutations: Arc<SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    outbox: Arc<dyn LifecycleOutbox>,
    notifier: OnceLock<Arc<dyn LifecycleFactNotifier>>,
    supervisor_started: AtomicBool,
    wakeup: Arc<tokio::sync::Notify>,
}

fn closed_runtime_facts(
    clock: &dyn awaken_time::WallClock,
    session: &mut SessionAggregate,
    plan: SessionActivitySettlementPlan,
    owner: &str,
    session_id: &awaken_session_types::SessionId,
) -> Result<Vec<ManagedLifecycleFact>, SessionActivityError> {
    if plan != SessionActivitySettlementPlan::CloseRunningInterval {
        return Ok(Vec::new());
    }
    Ok(session
        .activity()
        .finish_running(clock.now_ms())
        .map_err(|error| SessionActivityError::Unavailable(error.to_string()))?
        .map(|interval| {
            ManagedLifecycleFact::runtime_interval_closed(owner, session_id.as_str(), interval)
        })
        .into_iter()
        .collect::<Vec<_>>())
}

impl SessionLifecycle {
    pub(crate) fn new(
        mutations: Arc<SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
        outbox: Arc<dyn LifecycleOutbox>,
    ) -> Self {
        Self {
            mutations,
            clock,
            outbox,
            notifier: OnceLock::new(),
            supervisor_started: AtomicBool::new(false),
            wakeup: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn claim_supervisor(&self) -> bool {
        crate::claim_once(&self.supervisor_started)
    }

    /// Ask the sole supervisor to reread durable work; this grants no authority.
    pub fn request_reconciliation(&self) {
        self.wakeup.notify_one();
    }

    /// A notification-only port over this same signal. Retaining the port
    /// keeps no application, mutation authority, repository or clock alive.
    pub fn reconciliation_wakeup(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wakeup = self.wakeup.clone();
        Arc::new(move || wakeup.notify_one())
    }

    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.wakeup.notified()
    }

    /// Install the one existing outbox consumer's wake port exactly once.
    pub fn set_notifier(
        &self,
        notifier: Arc<dyn LifecycleFactNotifier>,
    ) -> Result<(), &'static str> {
        self.notifier
            .set(notifier)
            .map_err(|_| "Session lifecycle notifier is already installed")
    }

    pub fn notify_fact(&self) {
        if let Some(notifier) = self.notifier.get() {
            notifier.notify();
        }
    }

    #[must_use]
    pub fn outbox_handle(&self) -> Arc<dyn LifecycleOutbox> {
        self.outbox.clone()
    }

    /// Commit the hidden delete intent and Resource release intent atomically.
    pub async fn commit_delete_intent(
        &self,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let mutations = &self.mutations;
        let session_id = command.session_id.as_str();
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(&awaken_session_types::SessionId::from(session_id))
                .await
                .map_err(mutation_failure)?;
            let mut session = mutations
                .session_aggregate(&awaken_session_types::SessionId::from(session_id))
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            // The aggregate owns the complete Delete admission rule. Calling
            // it before the pending-batch gate makes a hidden durable replay
            // absorbing without duplicating disposition interpretation here;
            // a rejected first request mutates only this detached candidate.
            let Some(delete_fact) = session
                .lifecycle()
                .request_delete(
                    &owner_scope,
                    command.requested_at_unix_s.unwrap_or_else(|| {
                        i64::try_from(self.clock.now().duration_since_epoch().as_secs())
                            .unwrap_or(i64::MAX)
                    }),
                    self.clock.now_ms(),
                )
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?
            else {
                return Ok(SessionDispositionMutation::unchanged(
                    owner_scope,
                    session.into_snapshot(),
                ));
            };
            if session.has_incomplete_event_batches() {
                self.request_reconciliation();
                return Err(pending_event_batch_error());
            }
            let facts = vec![delete_fact];
            match mutations
                .commit_session_snapshot_with_outcome(&owner_scope, session, "delete-intent", facts)
                .await
            {
                Ok(commit) => {
                    return Ok(SessionDispositionMutation::committed(owner_scope, commit));
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    pub async fn reconcile_managed_budget_usage(
        &self,
        session_id: &awaken_session_types::SessionId,
        usage: SessionUsage,
    ) -> Result<BudgetSettlementOutcome, SessionMutationError> {
        let mutations = &self.mutations;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations.owner(session_id).await?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)?;
            let fallback_model = session
                .frozen_baseline()
                .map(|baseline| baseline.execution_model_ref.clone())
                .ok_or_else(|| {
                    SessionMutationError::Internal(
                        "Session baseline is not frozen while settling budget".into(),
                    )
                })?;
            let was_admissible = session.budget.can_admit_model_request();
            // Historical rows recorded the unconditional cursor only inside a
            // configured budget. Seed the new neutral root projection from that
            // same fact without inventing a migration ledger.
            session.budget().seed_legacy_usage();
            let active_seconds = session
                .usage_cursor
                .active_seconds
                .max(usage.active_seconds);
            let mut usage_cursor =
                ManagedBudgetUsageCursor::from_session_usage(&usage, Some(fallback_model.as_str()))
                    .map_err(|error| SessionMutationError::Internal(error.to_string()))?;
            usage_cursor.active_seconds = active_seconds;
            if !usage_cursor.is_at_least(&session.usage_cursor) {
                return Err(SessionMutationError::Internal(
                    "cumulative usage cannot move backwards".into(),
                ));
            }
            let usage_changed = usage_cursor != session.usage_cursor;
            let budget_usage_changed = session
                .budget()
                .reconcile_cumulative_usage(usage_cursor)
                .map_err(|error| SessionMutationError::Internal(error.to_string()))?;
            let reached_now = was_admissible && !session.budget.can_admit_model_request();
            let reach_transition = if reached_now {
                Some(
                    session
                        .budget()
                        .record_reach_transition()
                        .map_err(|error| SessionMutationError::Internal(error.to_string()))?
                        .ok_or_else(|| {
                            SessionMutationError::Internal(
                                "reached Session budget did not append transition provenance"
                                    .into(),
                            )
                        })?,
                )
            } else {
                None
            };
            if !usage_changed && !budget_usage_changed && !reached_now {
                return Ok(BudgetSettlementOutcome {
                    session: session.into_snapshot(),
                    reached_now: false,
                });
            }
            let lifecycle_facts = if let Some(transition) = &reach_transition {
                vec![ManagedLifecycleFact {
                    id: format!("budget-reached:{session_id}:{}", transition.generation),
                    object_id: session_id.to_string(),
                    workspace_id: Some(owner_scope.clone()),
                    event_type: "session.budget_reached".into(),
                    timestamp: i64::try_from(self.clock.now_ms() / 1_000).unwrap_or(i64::MAX),
                    runtime_interval: None,
                }]
            } else {
                Vec::new()
            };
            match mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "reconcile-managed-budget-usage",
                    lifecycle_facts,
                )
                .await
            {
                Ok(commit) => {
                    if reached_now {
                        self.notify_fact();
                    }
                    return Ok(BudgetSettlementOutcome {
                        session: commit.into_session(),
                        reached_now,
                    });
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(error),
            }
        }
        Err(SessionMutationError::Conflict)
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
        let mutations = &self.mutations;
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
            let settlement_plan =
                SessionActivitySettlementPlan::from_epoch_settlement(settlement, session.execution);
            if settlement_plan == SessionActivitySettlementPlan::Stutter {
                return Ok(session.into_snapshot());
            }
            // Completion order does not grant inverse-transition ownership.
            // Every admitted epoch is removed exactly once; only the removal of
            // the final active epoch may close the one shared Running interval.
            // Initial Worker realization may still be preparing/activating, in
            // which case settlement records the activity completion without
            // overwriting that stronger execution phase.
            let lifecycle_facts = closed_runtime_facts(
                self.clock.as_ref(),
                &mut session,
                settlement_plan,
                &owner_scope,
                session_id,
            )?;
            let emitted_runtime_interval = !lifecycle_facts.is_empty();
            match mutations
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
                        self.notify_fact();
                    }
                    return Ok(commit.into_session());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
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
        let mutations = &self.mutations;
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
            match mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "observe-runtime-boundary",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }
}

#[cfg(test)]
pub(crate) mod tests {

    #[tokio::test]
    async fn retained_reconciliation_hint_keeps_only_the_original_signal() {
        // L1 live lifecycle + retained hint => the original Notify receives its
        // permit. L2 lifecycle/mutations/store/clock dropped + retained hint =>
        // only that signal survives, with no retained command authority.
        // L3 hint dropped => signal released. This is one process-local signal,
        // not another scheduler, channel or durable lifecycle decision.
        use std::sync::Arc;
        let store = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let mutations = Arc::new(crate::SessionMutations::new(store.clone(), clock.clone()));
        let lifecycle = Arc::new(super::SessionLifecycle::new(
            mutations.clone(),
            clock.clone(),
            store.clone(),
        ));
        let weak_lifecycle = Arc::downgrade(&lifecycle);
        let weak_mutations = Arc::downgrade(&mutations);
        let weak_store = Arc::downgrade(&store);
        let weak_clock = Arc::downgrade(&clock);
        let signal = Arc::downgrade(&lifecycle.wakeup);
        let hint = lifecycle.reconciliation_wakeup();
        hint();
        tokio::time::timeout(std::time::Duration::from_secs(1), lifecycle.notified())
            .await
            .expect("L1 exact original wake permit");
        drop(lifecycle);
        drop(mutations);
        drop(store);
        drop(clock);
        assert!(
            weak_lifecycle.upgrade().is_none(),
            "L2 no lifecycle retention"
        );
        assert!(
            weak_mutations.upgrade().is_none(),
            "L2 no mutation retention"
        );
        assert!(weak_store.upgrade().is_none(), "L2 no repository retention");
        assert!(weak_clock.upgrade().is_none(), "L2 no clock retention");
        let retained = signal
            .upgrade()
            .expect("L2 only the original signal survives");
        hint();
        tokio::time::timeout(std::time::Duration::from_secs(1), retained.notified())
            .await
            .expect("L2 harmless signal after authority release");
        drop(retained);
        drop(hint);
        assert!(signal.upgrade().is_none(), "L3 no leaked signal");
    }

    /// B9 identity causes on the original construction test: command
    /// holders must share one Core/Clock and drain. A wake issued through the
    /// selected service must reach that original Notify, never another channel.
    pub(crate) async fn assert_composed_identity(app: &crate::SessionApplication) {
        let lifecycle = app.lifecycle();
        assert!(std::sync::Arc::ptr_eq(&app.lifecycle, &lifecycle));
        assert!(std::sync::Arc::ptr_eq(&app.mutations, &lifecycle.mutations));
        assert!(std::sync::Arc::ptr_eq(&app.clock, &lifecycle.clock));
        let repository = app.mutations.repository().clone();
        let outbox = lifecycle.outbox_handle();
        // Both trait-object handles come from one Store allocation. Compare the
        // allocation address, not distinct trait vtables or a synthetic marker.
        assert_eq!(
            std::sync::Arc::as_ptr(&repository) as *const (),
            std::sync::Arc::as_ptr(&outbox) as *const ()
        );
        lifecycle.request_reconciliation();
        tokio::time::timeout(std::time::Duration::from_secs(1), lifecycle.notified())
            .await
            .expect("original Notify retains the same wake permit");
    }
}
