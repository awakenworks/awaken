// @formal entrypoint ingress.commit_epoch_atomic_local
// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal entrypoint remote_worker.acknowledged_projection_is_monotonic
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
// @formal evidence session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
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
    Coordinator as CommitCoordinator, Error as CommitError, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitReceipt, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::RecoveryProjection;
use crate::{dispatch::ClaimedCommitCommand, dispatch::DispatchCommitAuthority};
pub use awaken_dispatch_contract::ClaimedRunCommit;
use awaken_dispatch_contract::CommitEpochGuard;
pub use awaken_dispatch_erasure::validate_commit_dispatch_binding;
use awaken_runtime_contract::runs::RunError;
use awaken_worker_contract::RunClaim;

/// Adapt the selected Coordinator writer to the SDK's execution error.
pub(crate) struct CoordinatorRunCommits(pub Arc<dyn ClaimedRunCommit<RunError>>);

#[async_trait]
impl ClaimedRunCommit for CoordinatorRunCommits {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        self.0
            .commit_operation(command)
            .await
            .map_err(|error| CommitError::Rejected(error.to_string()))
    }
}

/// Local claim fence around the authority selected at construction.
/// Versioned callers retain the actual operation port; a plain split reader
/// cannot acquire that capability by enabling a projection after erasure.
pub struct GuardedRunCommit {
    inner: CommitAuthority,
    store: Arc<dyn DispatchCommitAuthority>,
}

enum CommitAuthority {
    Plain(Arc<dyn CommitCoordinator>),
    Versioned(Arc<dyn OperationCoordinator>),
}

impl GuardedRunCommit {
    pub fn new(
        inner: Arc<dyn OperationCoordinator>,
        store: Arc<dyn DispatchCommitAuthority>,
    ) -> Self {
        Self {
            inner: CommitAuthority::Versioned(inner),
            store,
        }
    }

    /// Existing direct-reader composition supports only ordinary commits.
    pub fn from_plain(
        inner: Arc<dyn CommitCoordinator>,
        store: Arc<dyn DispatchCommitAuthority>,
    ) -> Self {
        Self {
            inner: CommitAuthority::Plain(inner),
            store,
        }
    }

    async fn guard(
        &self,
        claim: &RunClaim,
        commit: &ThreadCommit,
    ) -> Result<CommitEpochGuard, CommitError> {
        let guard = self
            .store
            .lock_commit_epoch(claim)
            .await
            .map_err(|error| CommitError::Rejected(format!("commit claim lock failed: {error}")))?
            .ok_or_else(|| fenced_error(claim))?;
        // Validation failures also pass through explicit asynchronous release.
        // No caller may observe completion while the exact claim is still held.
        if let Err(error) = validate_commit_dispatch_binding(guard.request(), commit) {
            return release_commit_guard(guard, Err(CommitError::Rejected(error))).await;
        }
        Ok(guard)
    }
}

#[async_trait]
impl ClaimedRunCommit for GuardedRunCommit {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        let coordinator: &dyn CommitCoordinator = match &self.inner {
            CommitAuthority::Plain(inner) => inner.as_ref(),
            CommitAuthority::Versioned(inner) => inner.as_ref(),
        };
        let guard = self.guard(claim, &commit).await?;
        release_commit_guard(guard, coordinator.commit(commit).await).await
    }

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        let CommitAuthority::Versioned(inner) = &self.inner else {
            return Err(CommitError::Rejected(
                "plain claimed commit authority does not support versioned CommitOperation".into(),
            ));
        };
        let guard = self
            .guard(&command.claim, &command.operation.commit)
            .await?;
        release_commit_guard(guard, inner.commit_operation(command.operation).await).await
    }
}

async fn release_commit_guard<T>(
    guard: CommitEpochGuard,
    committed: Result<T, CommitError>,
) -> Result<T, CommitError> {
    match guard.release_after(committed).await {
        Ok(committed) => committed,
        Err(error) => Err(CommitError::Rejected(format!(
            "commit claim lock release failed: {error}"
        ))),
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

#[cfg(test)]
#[path = "commit_fence/tests.rs"]
mod tests;
