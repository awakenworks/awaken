// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Shared composition admission and the original committed-read port binding; concrete
// @formal reason: co-located execution and SDK error projection belong to the launcher.
//! Exact authority/startup admission and committed-owner binding for both roots.
use awaken_session_application::SessionApplication;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionCompositionError {
    #[error("Session application observes another dispatch authority")]
    DispatchObservationMismatch,
    #[error("compose Session execution before starting dispatch services")]
    DispatchAlreadyStarted,
}

/// Both placement roots use this same guard before selecting service callbacks.
pub fn verify_composition(
    process: &awaken_dispatch_execution::ProcessDispatch,
    dispatch: &Arc<dyn awaken_dispatch::DispatchOperationalFeed>,
    sessions: &SessionApplication,
) -> Result<(), ExecutionCompositionError> {
    sessions
        .execution_admission()
        .verify_dispatch(dispatch)
        .map_err(|_| ExecutionCompositionError::DispatchObservationMismatch)?;
    if process.has_started_services() {
        return Err(ExecutionCompositionError::DispatchAlreadyStarted);
    }
    Ok(())
}

/// Bind only the selected read owner. ClaimedCommitService remains the sole
/// writer through this port; the adapter holds no execution composition.
pub struct SessionCommitOwner(pub Arc<awaken_session_run_delivery::CommittedReads>);

#[async_trait::async_trait]
impl awaken_dispatch::ClaimedOperationCoordinatorResolver for SessionCommitOwner {
    async fn resolve(
        &self,
        thread: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<
        Arc<dyn awaken_agent_contract::thread::commit::coordinator::OperationCoordinator>,
        awaken_session_execution_contract::ApplicationError,
    > {
        self.0.commit_owner(thread).await
    }
}
