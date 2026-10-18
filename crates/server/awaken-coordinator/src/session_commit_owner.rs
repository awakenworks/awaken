//! The Coordinator's claimed-commit service selects the Session's existing
//! commit owner through this shared composition binding to the Host.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_dispatch::ClaimedOperationCoordinatorResolver;
use awaken_session_executor::SharedHost;
use awaken_session_execution_contract::ApplicationError;

/// Binds [`ClaimedOperationCoordinatorResolver`] to
/// [`awaken_session_executor::session_commit_owner`]. The Host only resolves the existing
/// owner; `ClaimedCommitService` remains the sole caller of its write method.
pub struct SessionCommitOwner(Arc<SharedHost>);

impl SessionCommitOwner {
    /// Select the Host already installed by the Coordinator composition.
    #[must_use]
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self(host)
    }
}

#[async_trait::async_trait]
impl ClaimedOperationCoordinatorResolver for SessionCommitOwner {
    async fn resolve(
        &self,
        session_thread_id: &ThreadId,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        awaken_session_executor::session_commit_owner(&self.0, session_thread_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_binding_preserves_host_owner_selection() {
        // Causes: one selected Host and a non-resident Session Thread. Rule R1:
        // the shared binding retains that exact Host and resolves its configured
        // commit boundary without installing a resident Session. Existing
        // commit_ingest tests own valid/stale claims,
        // child partitioning, replay and persistence. This test covers only
        // the newly shared binding, not a second implementation of those rules.
        let host = awaken_runtime_host_testkit::volatile_host(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            "stub",
        )
        .await;
        let thread = ThreadId("unknown-shared-commit-session".into());
        let binding = SessionCommitOwner::new(host.clone());
        assert!(Arc::ptr_eq(&binding.0, &host));
        assert!(
            host.capabilities()
                .registered_thread_workspace(&thread.0)
                .is_none()
        );
        awaken_session_executor::session_commit_owner(&host, &thread)
            .await
            .expect("configured Host commit boundary");
        binding
            .resolve(&thread)
            .await
            .expect("shared binding reuses the Host boundary");
        assert!(
            host.capabilities()
                .registered_thread_workspace(&thread.0)
                .is_none()
        );
    }
}
