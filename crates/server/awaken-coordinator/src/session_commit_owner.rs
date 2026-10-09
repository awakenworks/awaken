//! The Coordinator's claimed-commit service selects the Session's existing
//! commit owner through this composition-local binding to the Host.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_dispatch::ClaimedOperationCoordinatorResolver;
use awaken_session_executor::SharedHost;
use awaken_session_execution_contract::ApplicationError;

/// Binds [`ClaimedOperationCoordinatorResolver`] to
/// [`awaken_session_executor::session_commit_owner`]. The Host only resolves the existing
/// owner; `ClaimedCommitService` remains the sole caller of its write method.
pub(crate) struct SessionCommitOwner(Arc<SharedHost>);

impl SessionCommitOwner {
    pub(crate) fn new(host: Arc<SharedHost>) -> Self {
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
