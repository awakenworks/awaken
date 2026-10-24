// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Durable operation DTOs and SPI declarations only; they carry no queue or
// @formal reason: stop authority. Coordinator Ingress owns the single claim-fenced implementation.
//! Neutral durable-Run operation values and the Host-implemented application
//! port the Coordinator's private durable operations API drives.

use awaken_agent_contract::agent::message::Message;

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
pub struct DurableDispatchStatus {
    pub run_id: String,
    pub status: String,
    pub attempts: u64,
    pub sandbox_bound: bool,
}

/// Coordinator application dependency for the private durable operations API.
///
/// The operational role keeps monitoring and control, while product command
/// admission belongs to the existing Session application roles. Cause/effect
/// rules O0-O2: the same receiver/import compiles for a lawful read (O0), but
/// cannot select either retired Legacy command (O1/O2), before any queue effect.
///
/// ```
/// use awaken_session_execution_contract::DurableRunOperations;
/// async fn operational_read_remains_available(operations: &dyn DurableRunOperations) {
///     let _ = operations.superseded("thread").await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_execution_contract::DurableRunOperations;
/// async fn no_legacy_supersede(operations: &dyn DurableRunOperations) {
///     let _ = operations.supersede(None, "thread", Vec::new()).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_execution_contract::DurableRunOperations;
/// async fn no_legacy_background(operations: &dyn DurableRunOperations) {
///     let _ = operations.submit_background(None, "thread", Vec::new()).await;
/// }
/// ```
#[async_trait::async_trait]
pub trait DurableRunOperations: Send + Sync {
    async fn cancel(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn pause(&self, thread: &str, run_id: Option<&str>) -> Result<String, ApplicationError>;
    async fn resume(&self, thread: &str, text: String) -> Result<String, ApplicationError>;
    async fn wake(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn deliver(&self, thread: &str, allow: bool) -> Result<String, ApplicationError>;
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
