//! Live `PendingBoundaryWake` delivery for `Mailbox`.
//!
//! `wake` is a live-only control operation: there is no durable fallback.
//! Callers that need a fail-closed guarantee on direct ingress should treat
//! `Ok(false)` as a definitive "no live subscriber" signal and return an
//! appropriate HTTP error rather than silently dropping the request.

use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::{
    LiveDeliveryOutcome, LiveRunCommand, LiveRunTarget, RunDispatchStatus,
};

use super::{Mailbox, MailboxError, live_target_for_dispatch, live_target_for_run};

impl Mailbox {
    /// Deliver a `PendingBoundaryWake` to the run identified by `id`.
    ///
    /// `id` is resolved in order: executor in-process lookup (run_id or
    /// thread_id), dispatch store (dispatch_id / correlation_id), run store
    /// (run_id), then run store latest (thread_id).
    ///
    /// Returns `Ok(true)` when the command was accepted by the live subscriber,
    /// `Ok(false)` when no matching active run or live subscriber was found.
    /// Wake has no durable fallback — callers on direct ingress that need
    /// fail-closed semantics must treat `Ok(false)` as a hard error.
    pub async fn wake(&self, id: &str) -> Result<bool, MailboxError> {
        if self.executor.wake_pending_boundary(id) {
            return Ok(true);
        }

        if let Some(dispatch) = self.store.load_dispatch(id).await?
            && dispatch.status() == RunDispatchStatus::Claimed
        {
            return self
                .deliver_live_wake(&live_target_for_dispatch(&dispatch))
                .await;
        }

        let run = if let Some(run) = self.run_store.load_run(id).await? {
            Some(run)
        } else {
            self.run_store.latest_run(id).await?
        };
        if let Some(run) = run
            && run.status == RunStatus::Running
        {
            return self.deliver_live_wake(&live_target_for_run(&run)).await;
        }

        Ok(false)
    }

    async fn deliver_live_wake(&self, target: &LiveRunTarget) -> Result<bool, MailboxError> {
        match self
            .store
            .deliver_live_to(target, LiveRunCommand::PendingBoundaryWake)
            .await?
        {
            LiveDeliveryOutcome::Delivered => Ok(true),
            LiveDeliveryOutcome::NoSubscriber => Ok(false),
        }
    }
}
