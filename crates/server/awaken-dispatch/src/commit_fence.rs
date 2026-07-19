//! Atomic commit authority for one claimed Run.
//!
//! A dispatch claim and a thread commit belong to separate aggregates, so the
//! durable worker uses this application service to coordinate their one shared
//! invariant: only the current claim may append Run truth. Local stores keep an
//! epoch guard alive across the real commit; remote workers inject a service that
//! performs the same check and commit in one server-side request.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::dispatch::{DispatchQueue, RunClaim};

/// Atomically apply a [`ThreadCommit`] under one durable [`RunClaim`].
#[async_trait]
pub trait ClaimedRunCommit: Send + Sync {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError>;
}

/// Local implementation: acquire the store's exact claim guard, keep it alive
/// across the ordinary thread commit, and fail closed when the claim is stale.
pub struct GuardedRunCommit {
    inner: Arc<dyn CommitCoordinator>,
    store: Arc<dyn DispatchQueue>,
}

impl GuardedRunCommit {
    pub fn new(inner: Arc<dyn CommitCoordinator>, store: Arc<dyn DispatchQueue>) -> Self {
        Self { inner, store }
    }
}

#[async_trait]
impl ClaimedRunCommit for GuardedRunCommit {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        let guard =
            self.store.lock_commit_epoch(claim).await.map_err(|error| {
                CommitError::Rejected(format!("commit claim lock failed: {error}"))
            })?;
        let Some(_guard) = guard else {
            return Err(fenced_error(claim));
        };
        // `_guard` intentionally remains alive across the await.
        self.inner.commit(commit).await
    }
}

/// Per-attempt coordinator installed in [`RuntimeRunContext`](awaken_runtime_contract::runtime_context::RuntimeRunContext).
/// It binds every step commit to the exact claim which admitted this attempt.
pub struct ClaimedCommitCoordinator {
    service: Arc<dyn ClaimedRunCommit>,
    claim: RunClaim,
}

impl ClaimedCommitCoordinator {
    pub fn new(service: Arc<dyn ClaimedRunCommit>, claim: RunClaim) -> Self {
        Self { service, claim }
    }
}

#[async_trait]
impl CommitCoordinator for ClaimedCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        self.service.commit(&self.claim, commit).await
    }
}

fn fenced_error(claim: &RunClaim) -> CommitError {
    CommitError::Rejected(format!(
        "run {} superseded: owner {:?} no longer holds lease epoch {}; the commit is fenced",
        claim.run_id.0, claim.owner, claim.epoch
    ))
}
