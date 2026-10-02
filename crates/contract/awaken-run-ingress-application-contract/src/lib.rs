// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Durable operation DTOs and SPI declarations only; they carry no queue or
// @formal reason: stop authority. Coordinator Ingress owns the single claim-fenced implementation.
//! Neutral durable-operation values and Host-implemented application ports.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use std::sync::Arc;

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

/// Select the existing physical commit owner from the Session Thread identity
/// already derived from a trusted, claim-guarded dispatch by Coordinator
/// application. The Coordinator retains the sole write invocation.
#[async_trait::async_trait]
pub trait ClaimedOperationCoordinatorResolver: Send + Sync {
    async fn resolve(
        &self,
        session_thread_id: &ThreadId,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError>;
}
