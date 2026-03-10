//! In-memory background task handle table and spawner.
//!
//! Manages the lifecycle of background tasks: spawn, track, cancel, query.
//! Epoch-based stale-completion guards prevent double updates. When a durable
//! [`TaskStore`] is configured, terminal summaries are persisted directly from
//! the manager and persistence failures remain visible on the live handle until
//! they are retried successfully.

use super::store::TaskStore;
use super::types::*;
use crate::loop_runtime::loop_runner::RunCancellationToken;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// In-memory runtime handle for a single background task.
#[derive(Debug)]
struct TaskHandle {
    epoch: u64,
    owner_thread_id: String,
    task_type: String,
    description: String,
    status: TaskStatus,
    error: Option<String>,
    result: Option<Value>,
    cancel_token: RunCancellationToken,
    cancellation_requested: bool,
    created_at_ms: u64,
    completed_at_ms: Option<u64>,
    parent_task_id: Option<TaskId>,
    metadata: Value,
    persistence_error: Option<String>,
}

/// Thread-scoped background task manager.
///
/// Manages the full lifecycle: spawn → track → cancel → query.
/// Tasks outlive individual runs but are scoped to a thread.
#[derive(Clone)]
pub struct BackgroundTaskManager {
    handles: Arc<Mutex<HashMap<TaskId, TaskHandle>>>,
    task_store: Option<Arc<TaskStore>>,
}

impl std::fmt::Debug for BackgroundTaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTaskManager")
            .field("task_count", &"<locked>")
            .finish()
    }
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            handles: Arc::new(Mutex::new(HashMap::new())),
            task_store: None,
        }
    }

    pub fn with_task_store(task_store: Option<Arc<TaskStore>>) -> Self {
        Self {
            handles: Arc::new(Mutex::new(HashMap::new())),
            task_store,
        }
    }

    /// Spawn a background task with the given closure.
    ///
    /// Returns the generated `TaskId` immediately. The closure receives a
    /// `CancellationToken` for cooperative cancellation.
    pub async fn spawn<F, Fut>(
        &self,
        owner_thread_id: &str,
        task_type: &str,
        description: &str,
        task: F,
    ) -> TaskId
    where
        F: FnOnce(RunCancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = TaskResult> + Send,
    {
        let params = SpawnParams {
            task_id: new_task_id(),
            owner_thread_id: owner_thread_id.to_string(),
            task_type: task_type.to_string(),
            description: description.to_string(),
            parent_task_id: None,
            metadata: Value::Object(serde_json::Map::new()),
        };
        self.spawn_impl(params, None, task).await
    }

    /// Spawn a background task with a caller-supplied ID and cancellation token.
    ///
    /// Use this when the caller already owns an ID (e.g. a sub-agent `run_id`)
    /// and/or a cancellation token that is shared with other subsystems.
    pub async fn spawn_with_id<F, Fut>(
        &self,
        params: SpawnParams,
        cancel_token: RunCancellationToken,
        task: F,
    ) -> TaskId
    where
        F: FnOnce(RunCancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = TaskResult> + Send,
    {
        self.spawn_impl(params, Some(cancel_token), task).await
    }

    async fn spawn_impl<F, Fut>(
        &self,
        params: SpawnParams,
        external_cancel_token: Option<RunCancellationToken>,
        task: F,
    ) -> TaskId
    where
        F: FnOnce(RunCancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = TaskResult> + Send,
    {
        let SpawnParams {
            task_id,
            owner_thread_id,
            task_type,
            description,
            parent_task_id,
            metadata,
        } = params;
        let cancel_token = external_cancel_token.unwrap_or_default();
        let now = now_ms();

        let epoch = {
            let mut handles = self.handles.lock().await;
            let epoch = handles.get(&task_id).map(|h| h.epoch + 1).unwrap_or(1);
            handles.insert(
                task_id.clone(),
                TaskHandle {
                    epoch,
                    owner_thread_id,
                    task_type,
                    description,
                    status: TaskStatus::Running,
                    error: None,
                    result: None,
                    cancel_token: cancel_token.clone(),
                    cancellation_requested: false,
                    created_at_ms: now,
                    completed_at_ms: None,
                    parent_task_id,
                    metadata,
                    persistence_error: None,
                },
            );
            epoch
        };

        let handles = self.handles.clone();
        let task_store = self.task_store.clone();
        let tid = task_id.clone();

        tokio::spawn(async move {
            let result = task(cancel_token).await;

            // Update in-memory handle.
            let completed_at = now_ms();
            {
                let mut map = handles.lock().await;
                if let Some(handle) = map.get_mut(&tid) {
                    if handle.epoch != epoch {
                        return; // Stale completion.
                    }
                    let status = if handle.cancellation_requested {
                        TaskStatus::Cancelled
                    } else {
                        result.status()
                    };
                    handle.status = status;
                    handle.error = match &result {
                        TaskResult::Failed(e) => Some(e.clone()),
                        _ => None,
                    };
                    handle.result = match &result {
                        TaskResult::Success(v) => Some(v.clone()),
                        _ => None,
                    };
                    handle.completed_at_ms = Some(completed_at);
                }
            }

            if let Some(task_store) = task_store {
                let maybe_summary = {
                    let map = handles.lock().await;
                    map.get(&tid)
                        .filter(|handle| handle.epoch == epoch)
                        .map(|handle| summary_from_handle(&tid, handle))
                };
                if let Some(summary) = maybe_summary {
                    let persistence_error = task_store
                        .persist_summary(&summary)
                        .await
                        .err()
                        .map(|e| e.to_string());
                    let mut map = handles.lock().await;
                    if let Some(handle) = map.get_mut(&tid) {
                        if handle.epoch != epoch {
                            return;
                        }
                        handle.persistence_error = persistence_error;
                    }
                }
            }
        });

        task_id
    }

    /// Cancel a task owned by the given thread. Returns `true` if cancelled.
    pub async fn cancel(&self, owner_thread_id: &str, task_id: &str) -> Result<(), String> {
        let mut handles = self.handles.lock().await;
        let Some(handle) = handles.get_mut(task_id) else {
            return Err(format!("Unknown task_id: {task_id}"));
        };
        if handle.owner_thread_id != owner_thread_id {
            return Err(format!("Unknown task_id: {task_id}"));
        }
        if handle.status != TaskStatus::Running {
            return Err(format!(
                "Task '{task_id}' is not running (current status: {})",
                handle.status.as_str()
            ));
        }
        handle.cancellation_requested = true;
        handle.cancel_token.cancel();
        Ok(())
    }

    /// Get a summary of a single task.
    pub async fn get(&self, owner_thread_id: &str, task_id: &str) -> Option<TaskSummary> {
        self.retry_persistence(owner_thread_id, Some(task_id)).await;
        let handles = self.handles.lock().await;
        let handle = handles.get(task_id)?;
        if handle.owner_thread_id != owner_thread_id {
            return None;
        }
        Some(summary_from_handle(task_id, handle))
    }

    /// List all tasks for a thread, optionally filtered by status.
    pub async fn list(
        &self,
        owner_thread_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Vec<TaskSummary> {
        self.retry_persistence(owner_thread_id, None).await;
        let handles = self.handles.lock().await;
        let mut out: Vec<TaskSummary> = handles
            .iter()
            .filter(|(_, h)| h.owner_thread_id == owner_thread_id)
            .filter(|(_, h)| status_filter.is_none_or(|s| s == h.status))
            .map(|(id, h)| summary_from_handle(id, h))
            .collect();
        out.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        out
    }

    /// Check if there are running tasks for a thread.
    pub async fn has_running_tasks(&self, owner_thread_id: &str) -> bool {
        let handles = self.handles.lock().await;
        handles
            .values()
            .any(|h| h.owner_thread_id == owner_thread_id && h.status == TaskStatus::Running)
    }

    /// Remove completed/terminal task entries from the in-memory table.
    pub async fn gc_terminal(&self, owner_thread_id: &str) -> usize {
        self.retry_persistence(owner_thread_id, None).await;
        let mut handles = self.handles.lock().await;
        let before = handles.len();
        handles.retain(|_, h| {
            h.owner_thread_id != owner_thread_id
                || !h.status.is_terminal()
                || h.persistence_error.is_some()
        });
        before - handles.len()
    }

    /// Check if a task exists for the given thread.
    pub async fn contains(&self, owner_thread_id: &str, task_id: &str) -> bool {
        let handles = self.handles.lock().await;
        handles
            .get(task_id)
            .is_some_and(|h| h.owner_thread_id == owner_thread_id)
    }

    /// Check if a task exists in any thread.
    pub async fn contains_any(&self, task_id: &str) -> bool {
        let handles = self.handles.lock().await;
        handles.contains_key(task_id)
    }

    /// Cancel a task and all its descendants (by `parent_task_id` chain).
    ///
    /// Returns summaries of all tasks that were cancelled. The root task must
    /// be owned by `owner_thread_id`; descendants are found by traversing
    /// `parent_task_id` links within the same owner.
    pub async fn cancel_tree(
        &self,
        owner_thread_id: &str,
        task_id: &str,
    ) -> Result<Vec<TaskSummary>, String> {
        let mut handles = self.handles.lock().await;
        let Some(root) = handles.get(task_id) else {
            return Err(format!("Unknown task_id: {task_id}"));
        };
        if root.owner_thread_id != owner_thread_id {
            return Err(format!("Unknown task_id: {task_id}"));
        }

        // Collect the full descendant tree.
        let tree_ids = collect_descendant_ids(&handles, owner_thread_id, task_id, true);
        if tree_ids.is_empty() {
            return Err(format!(
                "Task '{task_id}' is not running (current status: {})",
                root.status.as_str()
            ));
        }

        let mut cancelled = false;
        let mut out = Vec::with_capacity(tree_ids.len());
        for id in tree_ids {
            if let Some(handle) = handles.get_mut(&id) {
                if handle.status == TaskStatus::Running {
                    handle.cancellation_requested = true;
                    handle.cancel_token.cancel();
                    cancelled = true;
                }
                out.push(summary_from_handle(&id, handle));
            }
        }

        if cancelled {
            Ok(out)
        } else {
            Err(format!(
                "Task '{task_id}' is not running (current status: {})",
                handles
                    .get(task_id)
                    .map(|h| h.status.as_str())
                    .unwrap_or("unknown")
            ))
        }
    }

    /// Externally update a task's status (e.g. recovery marking orphans as stopped).
    pub async fn update_status(
        &self,
        task_id: &str,
        status: TaskStatus,
        error: Option<String>,
    ) -> bool {
        let mut handles = self.handles.lock().await;
        if let Some(handle) = handles.get_mut(task_id) {
            handle.status = status;
            handle.error = error;
            if status.is_terminal() {
                handle.completed_at_ms = Some(now_ms());
            }
            true
        } else {
            false
        }
    }

    /// List tasks filtered by `task_type`, optionally filtered by status.
    pub async fn list_by_type(
        &self,
        owner_thread_id: &str,
        task_type: &str,
        status_filter: Option<TaskStatus>,
    ) -> Vec<TaskSummary> {
        let handles = self.handles.lock().await;
        let mut out: Vec<TaskSummary> = handles
            .iter()
            .filter(|(_, h)| h.owner_thread_id == owner_thread_id)
            .filter(|(_, h)| h.task_type == task_type)
            .filter(|(_, h)| status_filter.is_none_or(|s| s == h.status))
            .map(|(id, h)| summary_from_handle(id, h))
            .collect();
        out.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        out
    }

    async fn retry_persistence(&self, owner_thread_id: &str, task_id: Option<&str>) {
        let Some(task_store) = self.task_store.as_ref().cloned() else {
            return;
        };

        let candidates: Vec<(TaskId, u64, TaskSummary)> = {
            let handles = self.handles.lock().await;
            handles
                .iter()
                .filter(|(id, handle)| {
                    handle.owner_thread_id == owner_thread_id
                        && handle.status.is_terminal()
                        && handle.persistence_error.is_some()
                        && task_id.is_none_or(|wanted| wanted == id.as_str())
                })
                .map(|(id, handle)| (id.clone(), handle.epoch, summary_from_handle(id, handle)))
                .collect()
        };

        for (task_id, epoch, summary) in candidates {
            let persistence_error = task_store
                .persist_summary(&summary)
                .await
                .err()
                .map(|e| e.to_string());
            let mut handles = self.handles.lock().await;
            if let Some(handle) = handles.get_mut(&task_id) {
                if handle.epoch != epoch || !handle.status.is_terminal() {
                    continue;
                }
                handle.persistence_error = persistence_error;
            }
        }
    }
}

/// Collect task IDs forming the subtree rooted at `root_id` via `parent_task_id` links.
fn collect_descendant_ids(
    handles: &HashMap<TaskId, TaskHandle>,
    owner_thread_id: &str,
    root_id: &str,
    include_root: bool,
) -> Vec<String> {
    // Build children-by-parent index.
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, h) in handles.iter() {
        if h.owner_thread_id != owner_thread_id {
            continue;
        }
        if let Some(parent) = h.parent_task_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(id.as_str());
        }
    }

    let mut result = Vec::new();
    let mut stack = vec![root_id];
    let mut is_root = true;
    while let Some(current) = stack.pop() {
        if !is_root || include_root {
            // Only include running tasks (for cancel_tree semantics).
            if handles
                .get(current)
                .is_some_and(|h| h.status == TaskStatus::Running)
            {
                result.push(current.to_string());
            }
        }
        is_root = false;
        if let Some(children) = children_by_parent.get(current) {
            for child in children {
                stack.push(child);
            }
        }
    }
    result
}

fn summary_from_handle(task_id: &str, handle: &TaskHandle) -> TaskSummary {
    TaskSummary {
        task_id: task_id.to_string(),
        task_type: handle.task_type.clone(),
        description: handle.description.clone(),
        status: handle.status,
        error: handle.error.clone(),
        result: handle.result.clone(),
        result_ref: None,
        created_at_ms: handle.created_at_ms,
        completed_at_ms: handle.completed_at_ms,
        parent_task_id: handle.parent_task_id.clone(),
        supports_resume: handle.task_type == "agent_run",
        attempt: 0,
        metadata: handle.metadata.clone(),
        persistence_error: handle.persistence_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::storage::{
        MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, ThreadHead, ThreadListPage,
        ThreadListQuery, ThreadReader, ThreadStore, ThreadStoreError, ThreadWriter,
        VersionPrecondition,
    };
    use crate::contracts::thread::{Thread, ThreadChangeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyTaskThreadStore {
        inner: Arc<tirea_store_adapters::MemoryStore>,
        fail_task_appends: AtomicUsize,
    }

    impl FlakyTaskThreadStore {
        fn new(fail_task_appends: usize) -> Arc<Self> {
            Arc::new(Self {
                inner: Arc::new(tirea_store_adapters::MemoryStore::new()),
                fail_task_appends: AtomicUsize::new(fail_task_appends),
            })
        }

        fn set_failures(&self, failures: usize) {
            self.fail_task_appends.store(failures, Ordering::SeqCst);
        }

        fn remaining_failures(&self) -> usize {
            self.fail_task_appends.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ThreadReader for FlakyTaskThreadStore {
        async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
            self.inner.load(thread_id).await
        }

        async fn list_threads(
            &self,
            query: &ThreadListQuery,
        ) -> Result<ThreadListPage, ThreadStoreError> {
            self.inner.list_threads(query).await
        }

        async fn load_messages(
            &self,
            thread_id: &str,
            query: &MessageQuery,
        ) -> Result<MessagePage, ThreadStoreError> {
            self.inner.load_messages(thread_id, query).await
        }

        async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
            self.inner.load_run(run_id).await
        }

        async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
            self.inner.list_runs(query).await
        }

        async fn active_run_for_thread(
            &self,
            thread_id: &str,
        ) -> Result<Option<RunRecord>, ThreadStoreError> {
            self.inner.active_run_for_thread(thread_id).await
        }
    }

    #[async_trait::async_trait]
    impl ThreadWriter for FlakyTaskThreadStore {
        async fn create(
            &self,
            thread: &Thread,
        ) -> Result<crate::contracts::storage::Committed, ThreadStoreError> {
            self.inner.create(thread).await
        }

        async fn append(
            &self,
            thread_id: &str,
            delta: &ThreadChangeSet,
            precondition: VersionPrecondition,
        ) -> Result<crate::contracts::storage::Committed, ThreadStoreError> {
            if thread_id.starts_with(TASK_THREAD_PREFIX)
                && self
                    .fail_task_appends
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        if remaining > 0 {
                            Some(remaining - 1)
                        } else {
                            None
                        }
                    })
                    .is_ok()
            {
                return Err(ThreadStoreError::Io(std::io::Error::other(
                    "injected task persistence failure",
                )));
            }
            self.inner.append(thread_id, delta, precondition).await
        }

        async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
            self.inner.delete(thread_id).await
        }

        async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
            self.inner.save(thread).await
        }
    }

    #[tokio::test]
    async fn spawn_and_complete_success() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "echo hello", |_cancel| async {
                TaskResult::Success(serde_json::json!({ "exit_code": 0 }))
            })
            .await;

        // Wait for the spawned task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.expect("task should exist");
        assert_eq!(summary.status, TaskStatus::Completed);
        assert!(summary.result.is_some());
        assert!(summary.error.is_none());
        assert!(summary.completed_at_ms.is_some());
    }

    #[tokio::test]
    async fn spawn_and_complete_failure() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "bad cmd", |_cancel| async {
                TaskResult::Failed("command not found".into())
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Failed);
        assert_eq!(summary.error.as_deref(), Some("command not found"));
    }

    #[tokio::test]
    async fn cancel_running_task() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "long running", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        // Task should be running.
        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Running);

        // Cancel it.
        mgr.cancel("thread-1", &tid).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_wrong_owner_rejected() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "task", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        let result = mgr.cancel("thread-other", &tid).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_filters_by_owner_and_status() {
        let mgr = BackgroundTaskManager::new();

        // Running task for thread-1.
        let _t1 = mgr
            .spawn("thread-1", "shell", "task-a", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        // Completed task for thread-1.
        mgr.spawn("thread-1", "http", "task-b", |_cancel| async {
            TaskResult::Success(Value::Null)
        })
        .await;

        // Task for thread-2 (should not appear).
        mgr.spawn("thread-2", "shell", "task-c", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let all = mgr.list("thread-1", None).await;
        assert_eq!(all.len(), 2);

        let running = mgr.list("thread-1", Some(TaskStatus::Running)).await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_type, "shell");

        let completed = mgr.list("thread-1", Some(TaskStatus::Completed)).await;
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task_type, "http");
    }

    #[tokio::test]
    async fn has_running_tasks_reflects_state() {
        let mgr = BackgroundTaskManager::new();

        assert!(!mgr.has_running_tasks("thread-1").await);

        let tid = mgr
            .spawn("thread-1", "shell", "task", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        assert!(mgr.has_running_tasks("thread-1").await);

        mgr.cancel("thread-1", &tid).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!mgr.has_running_tasks("thread-1").await);
    }

    #[tokio::test]
    async fn gc_terminal_removes_completed_tasks() {
        let mgr = BackgroundTaskManager::new();

        mgr.spawn("thread-1", "shell", "done", |_| async {
            TaskResult::Success(Value::Null)
        })
        .await;

        let _running = mgr
            .spawn("thread-1", "shell", "still going", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let removed = mgr.gc_terminal("thread-1").await;
        assert_eq!(removed, 1);

        let all = mgr.list("thread-1", None).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, TaskStatus::Running);
    }

    #[tokio::test]
    async fn get_returns_none_for_wrong_owner() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "task", |_| async {
                TaskResult::Success(Value::Null)
            })
            .await;

        assert!(mgr.get("thread-other", &tid).await.is_none());
        assert!(mgr.get("thread-1", &tid).await.is_some());
    }

    #[tokio::test]
    async fn cancel_already_completed_returns_error() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "fast", |_| async {
                TaskResult::Success(Value::Null)
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let err = mgr.cancel("thread-1", &tid).await.unwrap_err();
        assert!(err.contains("not running"));
    }

    // -----------------------------------------------------------------------
    // Concurrency & edge-case tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn many_concurrent_spawns_all_tracked() {
        let mgr = BackgroundTaskManager::new();
        let mut ids = Vec::new();

        for i in 0..20 {
            let desc = format!("task-{i}");
            let tid = mgr
                .spawn("thread-1", "shell", &desc, |cancel| async move {
                    cancel.cancelled().await;
                    TaskResult::Cancelled
                })
                .await;
            ids.push(tid);
        }

        let all = mgr.list("thread-1", None).await;
        assert_eq!(all.len(), 20);

        // All tasks should be running.
        let running = mgr.list("thread-1", Some(TaskStatus::Running)).await;
        assert_eq!(running.len(), 20);

        // Cancel all.
        for tid in &ids {
            mgr.cancel("thread-1", tid).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let cancelled = mgr.list("thread-1", Some(TaskStatus::Cancelled)).await;
        assert_eq!(cancelled.len(), 20);
    }

    #[tokio::test]
    async fn concurrent_cancel_and_complete_race() {
        // Task completes very quickly; cancel may arrive after completion.
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "fast", |_| async {
                TaskResult::Success(Value::Null)
            })
            .await;

        // Race: try to cancel immediately (may or may not succeed).
        let cancel_result = mgr.cancel("thread-1", &tid).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.unwrap();
        // Either cancelled (if we beat the completion) or completed (if not).
        assert!(
            summary.status == TaskStatus::Cancelled || summary.status == TaskStatus::Completed,
            "unexpected status: {:?}",
            summary.status
        );

        // If cancel succeeded, status should be Cancelled.
        if cancel_result.is_ok() {
            assert_eq!(summary.status, TaskStatus::Cancelled);
        }
    }

    #[tokio::test]
    async fn task_with_tokio_select_respects_cancellation() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "select-based", |cancel| async move {
                tokio::select! {
                    _ = cancel.cancelled() => TaskResult::Cancelled,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                        TaskResult::Success(Value::Null)
                    }
                }
            })
            .await;

        assert!(mgr.has_running_tasks("thread-1").await);

        mgr.cancel("thread-1", &tid).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Cancelled);
        assert!(!mgr.has_running_tasks("thread-1").await);
    }

    #[tokio::test]
    async fn task_failure_with_panic_safety() {
        // A task that returns a failure result (not a panic).
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "http", "timeout", |_| async {
                TaskResult::Failed("connection timed out".into())
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Failed);
        assert_eq!(summary.error.as_deref(), Some("connection timed out"));
        assert!(summary.completed_at_ms.is_some());
    }

    #[tokio::test]
    async fn gc_only_affects_specified_thread() {
        let mgr = BackgroundTaskManager::new();

        // Completed task on thread-1.
        mgr.spawn("thread-1", "shell", "done-1", |_| async {
            TaskResult::Success(Value::Null)
        })
        .await;
        // Completed task on thread-2.
        mgr.spawn("thread-2", "shell", "done-2", |_| async {
            TaskResult::Success(Value::Null)
        })
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // GC only thread-1.
        let removed = mgr.gc_terminal("thread-1").await;
        assert_eq!(removed, 1);

        // Thread-2's task should still be there.
        let t2_tasks = mgr.list("thread-2", None).await;
        assert_eq!(t2_tasks.len(), 1);
    }

    #[tokio::test]
    async fn task_summary_has_timing_info() {
        let mgr = BackgroundTaskManager::new();
        let tid = mgr
            .spawn("thread-1", "shell", "timed", |_| async {
                TaskResult::Success(Value::Null)
            })
            .await;

        // While running, should have created_at but no completed_at.
        let running = mgr.get("thread-1", &tid).await.unwrap();
        assert!(running.created_at_ms > 0);
        // Note: task may have already completed since it's instant.

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let completed = mgr.get("thread-1", &tid).await.unwrap();
        assert!(completed.created_at_ms > 0);
        assert!(completed.completed_at_ms.is_some());
        assert!(completed.completed_at_ms.unwrap() >= completed.created_at_ms);
    }

    #[tokio::test]
    async fn list_returns_sorted_by_creation_time() {
        let mgr = BackgroundTaskManager::new();

        let t1 = mgr
            .spawn("thread-1", "shell", "first", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;
        // Small delays to guarantee distinct timestamps.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let t2 = mgr
            .spawn("thread-1", "shell", "second", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let t3 = mgr
            .spawn("thread-1", "shell", "third", |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        let tasks = mgr.list("thread-1", None).await;
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].task_id, t1);
        assert_eq!(tasks[1].task_id, t2);
        assert_eq!(tasks[2].task_id, t3);
    }

    #[tokio::test]
    async fn default_impl_without_task_store_still_tracks_terminal_tasks() {
        let mgr = BackgroundTaskManager::new();
        mgr.spawn("thread-1", "shell", "task", |_| async {
            TaskResult::Success(Value::Null)
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No panic, no error — just verify it works.
        let tasks = mgr.list("thread-1", None).await;
        assert_eq!(tasks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // spawn_with_id tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_with_id_uses_caller_supplied_id() {
        let mgr = BackgroundTaskManager::new();
        let token = RunCancellationToken::new();
        let tid = mgr
            .spawn_with_id(
                SpawnParams {
                    task_id: "my-custom-id".to_string(),
                    owner_thread_id: "thread-1".to_string(),
                    task_type: "agent_run".to_string(),
                    description: "agent:worker".to_string(),
                    parent_task_id: None,
                    metadata: serde_json::json!({}),
                },
                token,
                |_cancel: RunCancellationToken| async { TaskResult::Success(Value::Null) },
            )
            .await;

        assert_eq!(tid, "my-custom-id");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", "my-custom-id").await.unwrap();
        assert_eq!(summary.task_type, "agent_run");
        assert_eq!(summary.description, "agent:worker");
        assert_eq!(summary.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn spawn_with_id_uses_external_cancel_token() {
        let mgr = BackgroundTaskManager::new();
        let token = RunCancellationToken::new();
        let token_clone = token.clone();

        mgr.spawn_with_id(
            SpawnParams {
                task_id: "cancel-test".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "long task".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            token,
            |cancel: RunCancellationToken| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await;

        // Task should be running.
        let summary = mgr.get("thread-1", "cancel-test").await.unwrap();
        assert_eq!(summary.status, TaskStatus::Running);

        // Cancel via the external token directly.
        token_clone.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Task closure returns Cancelled, but cancellation_requested was not set
        // via manager.cancel(), so the status uses result.status() = Cancelled.
        let summary = mgr.get("thread-1", "cancel-test").await.unwrap();
        assert_eq!(summary.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn spawn_with_id_cancel_via_manager_works() {
        let mgr = BackgroundTaskManager::new();
        let token = RunCancellationToken::new();

        mgr.spawn_with_id(
            SpawnParams {
                task_id: "mgr-cancel".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:worker".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            token,
            |cancel: RunCancellationToken| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await;

        // Cancel via manager (sets cancellation_requested).
        mgr.cancel("thread-1", "mgr-cancel").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", "mgr-cancel").await.unwrap();
        assert_eq!(summary.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn manager_persists_terminal_state_to_task_store_when_configured() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(storage.clone() as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::store::NewTaskSpec {
                task_id: "persisted-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: Value::Object(serde_json::Map::new()),
            })
            .await
            .unwrap();

        let mgr = BackgroundTaskManager::with_task_store(Some(task_store.clone()));
        mgr.spawn_with_id(
            SpawnParams {
                task_id: "persisted-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            RunCancellationToken::new(),
            |_cancel: RunCancellationToken| async {
                TaskResult::Success(serde_json::json!({ "stdout": "hi" }))
            },
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let persisted = task_store
            .load_task("persisted-task")
            .await
            .unwrap()
            .expect("task should persist");
        assert_eq!(persisted.status, TaskStatus::Completed);
        assert_eq!(
            persisted.result,
            Some(serde_json::json!({ "stdout": "hi" }))
        );
    }

    #[tokio::test]
    async fn manager_exposes_persistence_error_and_gc_retains_terminal_task_until_persisted() {
        let storage = FlakyTaskThreadStore::new(0);
        let task_store = Arc::new(TaskStore::new(storage.clone() as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::store::NewTaskSpec {
                task_id: "flaky-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: Value::Object(serde_json::Map::new()),
            })
            .await
            .unwrap();
        storage.set_failures(10);

        let mgr = BackgroundTaskManager::with_task_store(Some(task_store));
        mgr.spawn_with_id(
            SpawnParams {
                task_id: "flaky-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            RunCancellationToken::new(),
            |_cancel: RunCancellationToken| async { TaskResult::Success(Value::Null) },
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", "flaky-task").await.unwrap();
        assert_eq!(summary.status, TaskStatus::Completed);
        assert!(summary.persistence_error.is_some());
        assert!(storage.remaining_failures() < 10);

        let removed = mgr.gc_terminal("thread-1").await;
        assert_eq!(removed, 0);
        assert!(mgr.get("thread-1", "flaky-task").await.is_some());
    }

    #[tokio::test]
    async fn manager_retries_failed_persistence_on_get_and_clears_error() {
        let storage = FlakyTaskThreadStore::new(0);
        let task_store = Arc::new(TaskStore::new(storage.clone() as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::store::NewTaskSpec {
                task_id: "retry-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: Value::Object(serde_json::Map::new()),
            })
            .await
            .unwrap();
        storage.set_failures(1);

        let mgr = BackgroundTaskManager::with_task_store(Some(task_store.clone()));
        mgr.spawn_with_id(
            SpawnParams {
                task_id: "retry-task".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            RunCancellationToken::new(),
            |_cancel: RunCancellationToken| async {
                TaskResult::Success(serde_json::json!({ "stdout": "done" }))
            },
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before_retry = task_store
            .load_task("retry-task")
            .await
            .unwrap()
            .expect("task should exist");
        assert_eq!(before_retry.status, TaskStatus::Running);

        let summary = mgr.get("thread-1", "retry-task").await.unwrap();
        assert!(summary.persistence_error.is_none());

        let after_retry = task_store
            .load_task("retry-task")
            .await
            .unwrap()
            .expect("task should exist");
        assert_eq!(after_retry.status, TaskStatus::Completed);
        assert_eq!(
            after_retry.result,
            Some(serde_json::json!({ "stdout": "done" }))
        );
    }
}
