use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::state::{BackgroundTaskStateSnapshot, PersistedTaskMeta};
use super::types::{
    CancellationHandle, CancellationToken, TaskId, TaskResult, TaskStatus, TaskSummary,
};

struct LiveTask {
    task_id: TaskId,
    owner_thread_id: String,
    task_type: String,
    description: String,
    status: TaskStatus,
    error: Option<String>,
    result: Option<serde_json::Value>,
    created_at_ms: u64,
    completed_at_ms: Option<u64>,
    cancel_handle: CancellationHandle,
    _join_handle: JoinHandle<()>,
}

/// Thread-scoped handle table for background tasks.
///
/// Spawns, tracks, cancels, and queries background tasks.
pub struct BackgroundTaskManager {
    tasks: Mutex<HashMap<TaskId, LiveTask>>,
    counter: AtomicU64,
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    fn next_task_id(&self) -> TaskId {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("bg_{n}")
    }

    /// Spawn a background task.
    ///
    /// The `task_fn` receives a `CancellationToken` and returns a `TaskResult`.
    pub async fn spawn<F, Fut>(
        self: &Arc<Self>,
        owner_thread_id: &str,
        task_type: &str,
        description: &str,
        task_fn: F,
    ) -> TaskId
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = TaskResult> + Send + 'static,
    {
        let task_id = self.next_task_id();
        let (cancel_handle, cancel_token) = CancellationHandle::new();
        let now = now_ms();

        let manager = Arc::clone(self);
        let tid = task_id.clone();

        let join_handle = tokio::spawn(async move {
            let result = task_fn(cancel_token).await;
            let completed_at = now_ms();
            let mut tasks = manager.tasks.lock().await;
            if let Some(task) = tasks.get_mut(&tid) {
                task.status = result.status();
                task.completed_at_ms = Some(completed_at);
                match &result {
                    TaskResult::Success(val) => {
                        task.result = Some(val.clone());
                    }
                    TaskResult::Failed(err) => {
                        task.error = Some(err.clone());
                    }
                    TaskResult::Cancelled => {}
                }
            }
        });

        let live = LiveTask {
            task_id: task_id.clone(),
            owner_thread_id: owner_thread_id.to_string(),
            task_type: task_type.to_string(),
            description: description.to_string(),
            status: TaskStatus::Running,
            error: None,
            result: None,
            created_at_ms: now,
            completed_at_ms: None,
            cancel_handle,
            _join_handle: join_handle,
        };

        self.tasks.lock().await.insert(task_id.clone(), live);
        task_id
    }

    /// Cancel a running task.
    pub async fn cancel(&self, task_id: &str) -> bool {
        let tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get(task_id)
            && !task.status.is_terminal()
        {
            task.cancel_handle.cancel();
            return true;
        }
        false
    }

    /// List all tasks for a given owner thread.
    pub async fn list(&self, owner_thread_id: &str) -> Vec<TaskSummary> {
        let tasks = self.tasks.lock().await;
        tasks
            .values()
            .filter(|t| t.owner_thread_id == owner_thread_id)
            .map(|t| TaskSummary {
                task_id: t.task_id.clone(),
                task_type: t.task_type.clone(),
                description: t.description.clone(),
                status: t.status,
                error: t.error.clone(),
                result: t.result.clone(),
                created_at_ms: t.created_at_ms,
                completed_at_ms: t.completed_at_ms,
            })
            .collect()
    }

    /// Get the summary of a specific task.
    pub async fn get(&self, task_id: &str) -> Option<TaskSummary> {
        let tasks = self.tasks.lock().await;
        tasks.get(task_id).map(|t| TaskSummary {
            task_id: t.task_id.clone(),
            task_type: t.task_type.clone(),
            description: t.description.clone(),
            status: t.status,
            error: t.error.clone(),
            result: t.result.clone(),
            created_at_ms: t.created_at_ms,
            completed_at_ms: t.completed_at_ms,
        })
    }

    /// Restore persisted task metadata into the in-memory manager for a thread.
    ///
    /// Only missing task IDs are inserted; existing live tasks are preserved.
    pub(crate) async fn restore_for_thread(
        &self,
        owner_thread_id: &str,
        snapshot: &BackgroundTaskStateSnapshot,
    ) {
        let mut tasks = self.tasks.lock().await;
        for (task_id, meta) in &snapshot.tasks {
            if tasks.contains_key(task_id) {
                continue;
            }

            if let Some(n) = task_id
                .strip_prefix("bg_")
                .and_then(|s| s.parse::<u64>().ok())
            {
                self.counter
                    .fetch_max(n.saturating_add(1), Ordering::Relaxed);
            }

            let (cancel_handle, _cancel_token) = CancellationHandle::new();
            let join_handle = tokio::spawn(async {});
            tasks.insert(
                task_id.clone(),
                LiveTask {
                    task_id: meta.task_id.clone(),
                    owner_thread_id: owner_thread_id.to_string(),
                    task_type: meta.task_type.clone(),
                    description: meta.description.clone(),
                    status: meta.status,
                    error: meta.error.clone(),
                    result: None,
                    created_at_ms: meta.created_at_ms,
                    completed_at_ms: meta.completed_at_ms,
                    cancel_handle,
                    _join_handle: join_handle,
                },
            );
        }
    }

    pub(crate) async fn persisted_snapshot(&self) -> HashMap<TaskId, PersistedTaskMeta> {
        let tasks = self.tasks.lock().await;
        tasks
            .values()
            .map(|t| {
                let meta = PersistedTaskMeta {
                    task_id: t.task_id.clone(),
                    task_type: t.task_type.clone(),
                    description: t.description.clone(),
                    status: t.status,
                    error: t.error.clone(),
                    created_at_ms: t.created_at_ms,
                    completed_at_ms: t.completed_at_ms,
                };
                (t.task_id.clone(), meta)
            })
            .collect()
    }
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

use awaken_contract::now_ms;
