//! The commit-side fence: a run's durable writes are gated by its lease epoch.
//!
//! [`settle`](crate::dispatch::DispatchQueue::settle) is already fenced — a stale
//! owner cannot finalize a run a peer reclaimed. But a run also commits *per step*
//! while it executes, and those writes went unfenced: a slow-but-alive owner whose
//! lease lapsed and was re-claimed under a higher epoch would keep committing to its
//! thread, double-applying side effects the reclaimer is also producing (the
//! `dispatch-fencing-token-gap`).
//!
//! [`FencedCommitCoordinator`] closes that. It wraps the durable commit boundary and,
//! before each write, asks the dispatch store whether this attempt still holds the
//! current epoch (see
//! [`holds_current_epoch`](crate::dispatch::DispatchQueue::holds_current_epoch)). A
//! superseded owner is rejected, so at most one owner's writes ever land — the commit
//! twin of the settle fence. A backend that cannot read the fence fails OPEN, so a
//! deployment whose store does not expose the epoch behaves exactly as before.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::dispatch::DispatchQueue;

/// A commit boundary fenced by the lease epoch its run was claimed under. Built per
/// drive (the epoch is per claim), it wraps the base commit coordinator and consults
/// `store` for the current fence before delegating each write. See the module docs.
pub struct FencedCommitCoordinator {
    inner: Arc<dyn CommitCoordinator>,
    store: Arc<dyn DispatchQueue>,
    run_id: RunId,
    epoch: u64,
}

impl FencedCommitCoordinator {
    /// Fence `inner` for `run_id` claimed under lease `epoch`, checking `store` for the
    /// current fence before each commit.
    pub fn new(
        inner: Arc<dyn CommitCoordinator>,
        store: Arc<dyn DispatchQueue>,
        run_id: RunId,
        epoch: u64,
    ) -> Self {
        Self {
            inner,
            store,
            run_id,
            epoch,
        }
    }
}

#[async_trait]
impl CommitCoordinator for FencedCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        // Check the fence right before delegating, to shrink the window in which a
        // reclaim could slip between the check and the write. A store that cannot read
        // the epoch fails open, so this never blocks a deployment whose backend does
        // not expose the fence.
        let holds = self
            .store
            .holds_current_epoch(&self.run_id, self.epoch)
            .await
            .map_err(|e| CommitError::Rejected(format!("commit fence check failed: {e}")))?;
        if !holds {
            return Err(CommitError::Rejected(format!(
                "run {} superseded: its lease epoch {} was re-claimed under a higher epoch; \
                 the commit is fenced so a stale owner cannot double-apply side effects",
                self.run_id.0, self.epoch
            )));
        }
        self.inner.commit(commit).await
    }
}
