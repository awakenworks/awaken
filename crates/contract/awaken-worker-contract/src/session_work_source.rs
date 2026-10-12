//! Dispatch-owned Session Work commands and leases required at composition.

#[async_trait::async_trait]
pub trait SessionWorkSource: Send + Sync {
    async fn enqueue_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, crate::WorkQueueError>;

    async fn wake_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, crate::WorkQueueError>;

    async fn retire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<Option<crate::WorkItem>, crate::WorkQueueError>;

    async fn release_session_work(
        &self,
        lease: &crate::SessionWorkLease,
    ) -> Result<bool, crate::WorkQueueError>;

    async fn acquire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<crate::SessionWorkLease>, crate::WorkQueueError>;

    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, crate::WorkQueueError>;
}
