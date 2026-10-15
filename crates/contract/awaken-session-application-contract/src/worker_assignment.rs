// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Session-owned projection and lease ports carry only Session coordinates, never Environment queue rows.
//! Required Session Worker assignment and lease observations.
use awaken_session_types::{RunError, SessionId, SessionWorkLease};

/// Session lifecycle intent. Enqueue reconciles; wake requires new admitted
/// work; retire follows the terminal fence. These operations never run a Worker.
#[async_trait::async_trait]
pub trait SessionWorkerAssignment: Send + Sync {
    async fn enqueue_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<String, RunError>;
    async fn wake_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<String, RunError>;
    async fn retire_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<(), RunError>;
}

/// Environment adapter over its canonical lease authority. Session decides
/// topology, terminal eligibility and revival before invoking this role.
#[async_trait::async_trait]
pub trait SessionWorkerLeases: Send + Sync {
    async fn acquire_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, RunError>;
    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, RunError>;
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, RunError>;
}
