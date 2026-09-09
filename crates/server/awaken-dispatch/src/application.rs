//! Protocol-neutral application ports used by Coordinator HTTP adapters.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{CommitReceipt, commit_payload_hash};

use crate::commit_fence::validate_commit_dispatch_binding;
use crate::{ClaimedCommitRequest, DispatchQueue, RunDispatch};

/// Trusted Coordinator composition resolves an exact running execution domain
/// before physical admission. Worker request JSON is never an implementation.
/// Implementations must verify provider provenance and the full incarnation;
/// returning a coordinate is not a stop receipt or permission to clear a slot.
/// Lookup is read-only and retryable: it must not launch or stop execution.
#[async_trait::async_trait]
pub trait WorkerExecutionBindingSource: Send + Sync {
    async fn binding_for(
        &self,
        worker: &crate::WorkerIdentity,
    ) -> Result<crate::AttemptExecutionBinding, ApplicationError>;
}

/// Coordinator-authorized whole-Worker stop, not user cancellation of one Run.
/// Only the application below can construct it, after exact Registry closure.
/// It carries the original persisted binding, never a newly observed replacement.
///
/// ```compile_fail
/// use awaken_dispatch::{AttemptExecutionBinding, ClosedWorkerExecution};
/// fn forge(binding: AttemptExecutionBinding) {
///     let _ = ClosedWorkerExecution { binding };
/// }
/// ```
pub struct ClosedWorkerExecution {
    binding: crate::AttemptExecutionBinding,
}

impl ClosedWorkerExecution {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }

    /// Trusted adapters call this only after observing exact local process exit.
    /// This provider reference supports audit/reobservation, not remote replay.
    pub fn observed_exit(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let evidence_ref = evidence_ref.into();
        if evidence_ref.trim().is_empty() {
            return Err(ApplicationError::invalid(
                "process exit requires provider evidence",
            ));
        }
        Ok(WorkerProcessStopStatus::Exited(ObservedWorkerProcessExit {
            binding: self.binding.clone(),
            evidence_ref,
        }))
    }
}

/// Subject-bound LOCAL exit evidence. No wire deserialization and no conversion
/// to whole-attempt acknowledgement. The provider still owns the physical fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWorkerProcessExit {
    binding: crate::AttemptExecutionBinding,
    evidence_ref: String,
}

impl ObservedWorkerProcessExit {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }
    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

/// Local process observation only. Neither value authorizes Dispatch retirement
/// or replay: independent model, MCP and Sandbox participants may remain active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerProcessStopStatus {
    Unknown,
    Exited(ObservedWorkerProcessExit),
}

/// Trusted, independent infrastructure control, selected by Coordinator startup.
/// Implementations must validate the provider and exact frozen process identity,
/// stop idempotently, preserve evidence, and never upgrade local exit to an ACK.
#[async_trait::async_trait]
pub trait WorkerExecutionStopSource: Send + Sync {
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError>;
}

/// Progress a blocked successor through the existing authorities, without a
/// recovery queue or a second copy of slot state. The caller bounds provider IO
/// and retries through ordinary admission. `None` means no stop was authorized.
/// Even `Some(Exited)` is NOT complete attempt quiescence and cannot clear a slot.
pub async fn stop_expired_predecessor(
    dispatch: &dyn DispatchQueue,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    successor: &crate::RunClaim,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    if !dispatch
        .claim_is_current(successor, now_ms)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    {
        return Ok(None);
    }
    let Some(predecessor) = dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    // Never kill the current caller's shared process for a per-Run conflict.
    if predecessor.claim.owner == successor.owner {
        return Ok(None);
    }
    let Some(binding) = &predecessor.binding else {
        return Ok(None); // Historical absence cannot be backfilled into proof.
    };
    binding
        .validate_for(&predecessor.claim)
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
    if directory
        .expire_incarnation(binding.worker(), now_ms)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        != crate::RegistryMutation::Applied
    {
        return Ok(None);
    }
    // The Registry transaction is over before any provider IO. A concurrent
    // normal ACK/handoff must not retarget this command to the successor slot.
    if dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .as_ref()
        != Some(&predecessor)
    {
        return Ok(None);
    }
    let observation = source
        .stop_closed_worker(&ClosedWorkerExecution {
            binding: binding.clone(),
        })
        .await?;
    if let WorkerProcessStopStatus::Exited(receipt) = &observation
        && receipt.binding() != binding
    {
        return Err(ApplicationError::internal(
            "provider exit observation names another execution",
        ));
    }
    Ok(Some(observation))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationErrorKind {
    InvalidRequest,
    Conflict,
    Internal,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApplicationError {
    pub kind: ApplicationErrorKind,
    pub message: String,
}

impl ApplicationError {
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::InvalidRequest,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::invalid(message)
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Conflict,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Internal,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Unavailable,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSupersedeResult {
    pub state: String,
    pub superseded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableDispatchStatus {
    pub run_id: String,
    pub status: String,
    pub attempts: u64,
    pub sandbox_bound: bool,
}

/// Coordinator application dependency for the private durable operations API.
#[async_trait::async_trait]
pub trait DurableRunOperations: Send + Sync {
    async fn submit_background(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<String, ApplicationError>;
    async fn cancel(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn pause(&self, thread: &str, run_id: Option<&str>) -> Result<String, ApplicationError>;
    async fn resume(&self, thread: &str, text: String) -> Result<String, ApplicationError>;
    async fn wake(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn deliver(&self, thread: &str, allow: bool) -> Result<String, ApplicationError>;
    async fn supersede(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<DurableSupersedeResult, ApplicationError>;
    async fn messages(&self, thread: &str) -> Result<Vec<Message>, ApplicationError>;
    async fn superseded(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<DurableDispatchStatus>, ApplicationError>;
    async fn reconcile(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn quarantine_retry_exhausted(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, ApplicationError>;
    async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn requeue_dead_letter(
        &self,
        thread: &str,
        run_id: &str,
    ) -> Result<bool, ApplicationError>;
    async fn purge_dead_letters(&self, thread: &str) -> Result<usize, ApplicationError>;
}

#[async_trait::async_trait]
pub trait ClaimedOperationCoordinatorResolver: Send + Sync {
    /// Resolve the physical Coordinator selected by the claim-guarded
    /// dispatch. `dispatch` is trusted queue truth, never Worker input. The
    /// resolver may select an authority but cannot replace the one canonical
    /// `OperationCoordinator::commit_operation` write boundary below.
    async fn resolve(
        &self,
        trusted_dispatch: &RunDispatch,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError>;
}

struct FixedOperationCoordinatorResolver(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl ClaimedOperationCoordinatorResolver for FixedOperationCoordinatorResolver {
    async fn resolve(
        &self,
        _dispatch: &RunDispatch,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        Ok(self.0.clone())
    }
}

/// Exact-epoch committed-truth application service. Authentication and HTTP
/// status mapping are deliberately outside this owner.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchQueue>,
    coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchQueue>,
        coordinator: Arc<dyn OperationCoordinator>,
    ) -> Self {
        Self {
            dispatch,
            coordinator: Arc::new(FixedOperationCoordinatorResolver(coordinator)),
        }
    }

    #[must_use]
    pub fn with_coordinator_resolver(
        dispatch: Arc<dyn DispatchQueue>,
        coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
    ) -> Self {
        Self {
            dispatch,
            coordinator,
        }
    }

    pub async fn apply_claimed(
        &self,
        request: ClaimedCommitRequest,
    ) -> Result<CommitReceipt, ApplicationError> {
        let guard = self
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .ok_or_else(|| ApplicationError::invalid("run claim is stale"))?;
        let trusted_dispatch = guard.request();
        validate_commit_dispatch_binding(trusted_dispatch, &request.operation.commit)
            .map_err(ApplicationError::invalid)?;
        if request.operation.operation_id.run_id != *trusted_dispatch.run_id() {
            return Err(ApplicationError::invalid(format!(
                "commit operation run {} does not match claimed dispatch run {}",
                request.operation.operation_id.run_id.0,
                trusted_dispatch.run_id().0
            )));
        }
        let expected_hash = commit_payload_hash(&request.operation.commit)
            .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        if expected_hash != request.operation.payload_hash {
            return Err(ApplicationError::invalid(
                "commit operation payload hash does not match ThreadCommit",
            ));
        }
        let coordinator = self.coordinator.resolve(trusted_dispatch).await?;
        let receipt = coordinator
            .commit_operation(request.operation)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        drop(guard);
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_error_kinds_preserve_http_mapping_causes() {
        // Cause/effect graph: C1 caller input/claim is invalid -> E1 4xx;
        // C2 state conflicts -> E2 409; C3 permanent internal failure -> E3 500;
        // C4 temporary dependency failure -> E4 retryable 503.
        // The application port preserves these causes without importing HTTP.
        //
        // | Rule | cause            | effect kind     |
        // | R1   | invalid request  | InvalidRequest  |
        // | R2   | state conflict   | Conflict        |
        // | R3   | internal fault   | Internal        |
        // | R4   | dependency down  | Unavailable     |
        assert_eq!(
            ApplicationError::invalid("x").kind,
            ApplicationErrorKind::InvalidRequest
        );
        assert_eq!(
            ApplicationError::conflict("x").kind,
            ApplicationErrorKind::Conflict
        );
        assert_eq!(
            ApplicationError::internal("x").kind,
            ApplicationErrorKind::Internal
        );
        assert_eq!(
            ApplicationError::unavailable("x").kind,
            ApplicationErrorKind::Unavailable
        );
    }
}
