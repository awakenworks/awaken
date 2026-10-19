// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal evidence session_budget.accepted_cost_arithmetic_never_wraps
// Budget settlement integrates the original pure contract kernel; this adds no
// production arithmetic entrypoint or heap/CAS proof. B10 owns the integration regression.
//! Durable Delete intent, cumulative budget facts and the existing lifecycle signals.
use crate::credentials::mutation_failure;
use crate::mutation::{ROOT_CAS_ATTEMPTS, repository_failure};
use crate::terminal::pending_event_batch_error;
use crate::{
    SessionDispositionMutation, SessionMutationError, SessionMutations, SessionPreparationError,
};
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_budget_contract::{ManagedBudgetUsageCursor, SessionUsage};
use awaken_session_repository_contract::LifecycleOutbox;
use awaken_session_state_contract::{ManagedLifecycleFact, PersistedSession};
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
/// use awaken_session_budget_contract::SessionUsage;
/// use awaken_session_types::SessionId;
/// async fn settle(service: &SessionLifecycle, id: &SessionId, usage: SessionUsage) {
///     let _ = service.reconcile_managed_budget_usage(id, usage).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// use awaken_session_budget_contract::SessionUsage;
/// use awaken_session_types::SessionId;
/// async fn old_command(app: &SessionApplication, id: &SessionId, usage: SessionUsage) {
///     let _ = app.reconcile_managed_budget_usage(id, usage).await;
/// }
/// ```
pub struct SessionLifecycle {
    mutations: Arc<SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    outbox: Arc<dyn LifecycleOutbox>,
    notifier: OnceLock<Arc<dyn LifecycleFactNotifier>>,
    supervisor_started: AtomicBool,
    wakeup: tokio::sync::Notify,
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
            wakeup: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn claim_supervisor(&self) -> bool {
        crate::claim_once(&self.supervisor_started)
    }

    /// Ask the sole supervisor to reread durable work; this grants no authority.
    pub fn request_reconciliation(&self) {
        self.wakeup.notify_one();
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
                    SessionMutationError::Unavailable(
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
                    .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
            usage_cursor.active_seconds = active_seconds;
            if !usage_cursor.is_at_least(&session.usage_cursor) {
                return Err(SessionMutationError::Unavailable(
                    "cumulative usage cannot move backwards".into(),
                ));
            }
            let usage_changed = usage_cursor != session.usage_cursor;
            let budget_usage_changed = session
                .budget()
                .reconcile_cumulative_usage(usage_cursor)
                .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
            let reached_now = was_admissible && !session.budget.can_admit_model_request();
            let reach_transition = if reached_now {
                Some(
                    session
                        .budget()
                        .record_reach_transition()
                        .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?
                        .ok_or_else(|| {
                            SessionMutationError::Unavailable(
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
}

#[cfg(test)]
pub(crate) mod tests {
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
