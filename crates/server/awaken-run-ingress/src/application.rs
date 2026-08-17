//! Protocol-neutral application ports used by Coordinator HTTP adapters.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};

use crate::{ClaimedCommitRequest, DispatchQueue, commit_payload_hash};

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
pub trait ClaimedCommitApplier: Send + Sync {
    async fn apply(&self, operation: CommitOperation) -> Result<CommitReceipt, ApplicationError>;
}

struct CoordinatorCommitApplier(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl ClaimedCommitApplier for CoordinatorCommitApplier {
    async fn apply(&self, operation: CommitOperation) -> Result<CommitReceipt, ApplicationError> {
        self.0
            .commit_operation(operation)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))
    }
}

/// Exact-epoch committed-truth application service. Authentication and HTTP
/// status mapping are deliberately outside this owner.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchQueue>,
    applier: Arc<dyn ClaimedCommitApplier>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchQueue>,
        coordinator: Arc<dyn OperationCoordinator>,
    ) -> Self {
        Self {
            dispatch,
            applier: Arc::new(CoordinatorCommitApplier(coordinator)),
        }
    }

    #[must_use]
    pub fn with_applier(
        dispatch: Arc<dyn DispatchQueue>,
        applier: Arc<dyn ClaimedCommitApplier>,
    ) -> Self {
        Self { dispatch, applier }
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
        let expected_hash = commit_payload_hash(&request.operation.commit)
            .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        if expected_hash != request.operation.payload_hash {
            return Err(ApplicationError::invalid(
                "commit operation payload hash does not match ThreadCommit",
            ));
        }
        let receipt = self.applier.apply(request.operation).await?;
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
