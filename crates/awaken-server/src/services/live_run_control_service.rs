//! Fail-closed live run control: cancel and wake by correlation-id.
//!
//! `LiveRunControlService` routes `Cancel` and `PendingBoundaryWake` commands
//! to the active run identified by a correlation identifier (dispatch_id,
//! run_id, or thread_id).  Both operations are fail-closed on direct ingress:
//!
//! - `cancel` returns [`LiveRunControlError::NotFound`] when no dispatch or
//!   run matches, and propagates live-delivery failures instead of silently
//!   returning `Ok(false)`.
//! - `wake` returns [`LiveRunControlError::NoSubscriber`] when no live
//!   subscriber accepted the `PendingBoundaryWake` command.  Wake has no
//!   durable fallback; a direct-ingress caller that receives this error must
//!   treat the operation as undelivered rather than silently succeeding.
//!
//! # Guardrail
//!
//! **A-G18: `LiveRunControl` cancel and wake are fail-closed for direct ingress.**
//! `cancel` surfaces `NotFound` when no dispatch or run matches; `wake` surfaces
//! `NoSubscriber` when no live subscriber accepts the command.  Neither operation
//! returns a silent success. Validation: tests in this module.

use std::sync::Arc;

use thiserror::Error;

use crate::app::RunModuleState;
use crate::mailbox::{Mailbox, MailboxError};

/// Errors produced by [`LiveRunControlService`] operations.
#[derive(Debug, Error)]
pub enum LiveRunControlError {
    /// No dispatch or run matched the supplied correlation identifier.
    #[error("run not found: {0}")]
    NotFound(String),
    /// The run exists but has no live subscriber to accept the command.
    ///
    /// Wake is live-only; durable-only operations are fail-closed on direct
    /// ingress when no subscriber is reachable.
    #[error("no live subscriber for run: {0}")]
    NoSubscriber(String),
    /// A storage or mailbox error prevented the operation from completing.
    #[error("mailbox error: {0}")]
    Mailbox(#[from] MailboxError),
}

/// Fail-closed live-control service for active runs.
///
/// Wraps [`Mailbox`] cancel and wake with strict direct-ingress semantics:
/// every `Ok(false)` from the mailbox is surfaced as a typed error rather
/// than silently swallowed.
#[derive(Clone)]
pub struct LiveRunControlService {
    state: RunModuleState,
}

impl LiveRunControlService {
    pub fn new(state: RunModuleState) -> Self {
        Self { state }
    }

    fn mailbox(&self) -> Arc<Mailbox> {
        self.state.mailbox()
    }

    /// Cancel the run or dispatch identified by `correlation_id`.
    ///
    /// The identifier is resolved in priority order: dispatch store (by
    /// dispatch_id / correlation_id), executor (by run_id or thread_id),
    /// then run store.  Returns [`LiveRunControlError::NotFound`] when no
    /// matching active target exists.
    pub async fn cancel(&self, correlation_id: &str) -> Result<(), LiveRunControlError> {
        let cancelled = self
            .mailbox()
            .cancel(&self.state.scoped_id(correlation_id))
            .await?;
        if cancelled {
            Ok(())
        } else {
            Err(LiveRunControlError::NotFound(correlation_id.to_string()))
        }
    }

    /// Deliver a `PendingBoundaryWake` to the run identified by `correlation_id`.
    ///
    /// The identifier is resolved against the executor (by run_id or
    /// thread_id), dispatch store (by dispatch_id), and run store.  Wake is a
    /// live-only operation; if no subscriber is found,
    /// [`LiveRunControlError::NoSubscriber`] is returned.  Direct-ingress
    /// callers must treat this as a hard failure — there is no durable
    /// fallback for wake.
    pub async fn wake(&self, correlation_id: &str) -> Result<(), LiveRunControlError> {
        let delivered = self
            .mailbox()
            .wake(&self.state.scoped_id(correlation_id))
            .await?;
        if delivered {
            Ok(())
        } else {
            Err(LiveRunControlError::NoSubscriber(
                correlation_id.to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use awaken_runtime::AgentRuntime;
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};

    use crate::app::RunModuleState;
    use crate::mailbox::{Mailbox, MailboxConfig};

    struct NullResolver;
    impl awaken_runtime::AgentResolver for NullResolver {
        fn resolve(
            &self,
            id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: id.to_string(),
            })
        }
    }

    fn make_service() -> LiveRunControlService {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(NullResolver)));
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            Arc::new(InMemoryMailboxStore::new()),
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));
        let state = RunModuleState::new(
            runtime,
            mailbox,
            Arc::new(InMemoryStore::new()),
            Arc::new(NullResolver),
        );
        LiveRunControlService::new(state)
    }

    #[tokio::test]
    async fn cancel_is_fail_closed_for_unknown_correlation_id() {
        let svc = make_service();
        let err = svc.cancel("nonexistent-correlation-id").await.unwrap_err();
        assert!(
            matches!(err, LiveRunControlError::NotFound(_)),
            "cancel must be fail-closed: got {err}"
        );
    }

    #[tokio::test]
    async fn wake_is_fail_closed_when_no_live_subscriber() {
        let svc = make_service();
        let err = svc.wake("nonexistent-correlation-id").await.unwrap_err();
        assert!(
            matches!(err, LiveRunControlError::NoSubscriber(_)),
            "wake must be fail-closed: got {err}"
        );
    }
}
