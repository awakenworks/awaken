// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
//! Durable Delete intent and the existing process-owned lifecycle signals.
use crate::credentials::mutation_failure;
use crate::mutation::{ROOT_CAS_ATTEMPTS, repository_failure};
use crate::terminal::pending_event_batch_error;
use crate::{
    SessionDispositionMutation, SessionMutationError, SessionMutations, SessionPreparationError,
};
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_repository_contract::LifecycleOutbox;
use awaken_time::WallClockExt as _;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

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
}

#[cfg(test)]
pub(crate) mod tests {
    /// Added identity causes on the original construction test: two command
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
