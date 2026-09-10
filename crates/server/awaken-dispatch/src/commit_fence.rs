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
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::dispatch::{ClaimedCommitCommand, DispatchQueue, RunClaim};
use crate::{RecoveryProjection, RunDispatch};
pub use awaken_dispatch_contract::ClaimedRunCommit;

/// Verify that committed logical Run/Thread truth belongs to the dispatch whose
/// exact claim is currently fenced. Both embedded and remote workers cross this
/// helper so topology cannot change the authority check.
pub(crate) fn validate_commit_dispatch_binding(
    dispatch: &RunDispatch,
    commit: &ThreadCommit,
) -> Result<(), String> {
    if commit.run_id() != dispatch.run_id() {
        return Err(format!(
            "commit run {} does not match claimed dispatch run {}",
            commit.run_id().0,
            dispatch.run_id().0
        ));
    }
    if &commit.thread_id != dispatch.thread_id() {
        return Err(format!(
            "commit thread {} does not match claimed dispatch thread {}",
            commit.thread_id.0,
            dispatch.thread_id().0
        ));
    }
    Ok(())
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
        let Some(guard) = guard else {
            return Err(fenced_error(claim));
        };
        validate_commit_dispatch_binding(guard.request(), &commit)
            .map_err(CommitError::Rejected)?;
        // Cause/effect decision table for the cross-aggregate lock boundary:
        // C1 the Thread commit succeeds/fails; C2 the backend guard releases
        // synchronously/asynchronously. Every rule keeps the guard across the
        // commit and awaits its exact release before returning. A release error
        // is surfaced even after an idempotent Thread commit so a caller cannot
        // mistake a still-locked dispatch row for an available recovery slot.
        // R1/R2 C1=* + C2=* => commit result only after release; R3 release
        // error => explicit release rejection, never early success.
        let committed = self.inner.commit(commit).await;
        match guard.release_after(committed).await {
            Ok(committed) => committed,
            Err(error) => Err(CommitError::Rejected(format!(
                "commit claim lock release failed: {error}"
            ))),
        }
    }
}

/// Per-attempt coordinator installed in [`RuntimeRunContext`](awaken_runtime_contract::runtime_context::RuntimeRunContext).
/// It binds every step commit to the exact claim which admitted this attempt.
pub struct ClaimedCommitCoordinator {
    service: Arc<dyn ClaimedRunCommit>,
    claim: RunClaim,
    projection: Option<Arc<RecoveryProjection>>,
}

impl ClaimedCommitCoordinator {
    pub fn new(service: Arc<dyn ClaimedRunCommit>, claim: RunClaim) -> Self {
        Self {
            service,
            claim,
            projection: None,
        }
    }

    #[must_use]
    pub fn with_recovery_projection(mut self, projection: Arc<RecoveryProjection>) -> Self {
        self.projection = Some(projection);
        self
    }
}

#[async_trait]
impl CommitCoordinator for ClaimedCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let Some(projection) = &self.projection else {
            return self.service.commit(&self.claim, commit).await;
        };
        let snapshot = projection.current().ok_or_else(|| {
            CommitError::Rejected("remote commit has no installed recovery snapshot".to_string())
        })?;
        if snapshot.thread_id != commit.thread_id || snapshot.claimed_run_id != *commit.run_id() {
            return Err(CommitError::Rejected(
                "remote commit does not match the installed recovery projection".to_string(),
            ));
        }
        let payload_hash = commit_payload_hash(&commit)
            .map_err(|error| CommitError::Rejected(error.to_string()))?;
        let operation = CommitOperation {
            operation_id: CommitOperationId::new(
                commit.run_id().clone(),
                snapshot.next_commit_ordinal,
            ),
            expected_thread_version: snapshot.thread_version,
            payload_hash,
            commit,
        };
        let receipt = self
            .service
            .commit_operation(ClaimedCommitCommand {
                claim: self.claim.clone(),
                operation: operation.clone(),
            })
            .await?;
        projection.apply_receipt(operation, &receipt)?;
        Ok(receipt.commit_record())
    }
}

fn fenced_error(claim: &RunClaim) -> CommitError {
    CommitError::Rejected(format!(
        "run {} superseded: owner {:?} no longer holds lease epoch {}; the commit is fenced",
        claim.run_id.0, claim.owner, claim.epoch
    ))
}
