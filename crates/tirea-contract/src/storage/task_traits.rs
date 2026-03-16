use async_trait::async_trait;

use super::{Task, TaskPage, TaskQuery, TaskStoreError, TaskUpdate};

#[async_trait]
pub trait TaskReader: Send + Sync {
    async fn load_task(&self, task_id: &str) -> Result<Option<Task>, TaskStoreError>;

    async fn list_tasks(&self, query: &TaskQuery) -> Result<TaskPage, TaskStoreError>;
}

#[async_trait]
pub trait TaskWriter: TaskReader {
    async fn create_task(&self, task: &Task) -> Result<(), TaskStoreError>;

    async fn update_task(
        &self,
        task_id: &str,
        update: &TaskUpdate,
        now: u64,
    ) -> Result<Task, TaskStoreError>;

    /// CAS claim: sets owner only if currently unowned and not blocked.
    async fn claim_task(
        &self,
        task_id: &str,
        agent_id: &str,
        now: u64,
    ) -> Result<Task, TaskStoreError>;

    /// Lead privilege: assigns owner unconditionally (overrides existing owner).
    async fn assign_task(
        &self,
        task_id: &str,
        agent_id: &str,
        now: u64,
    ) -> Result<Task, TaskStoreError>;

    /// Mark task completed and clear it from downstream `blocked_by` lists.
    async fn complete_task(&self, task_id: &str, now: u64) -> Result<Task, TaskStoreError>;

    async fn delete_task(&self, task_id: &str) -> Result<(), TaskStoreError>;
}

pub trait TaskStore: TaskWriter {}

impl<T: TaskWriter + ?Sized> TaskStore for T {}
