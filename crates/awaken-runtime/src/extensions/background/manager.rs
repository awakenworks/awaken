use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::cancellation::{CancellationHandle, CancellationToken};
use crate::inbox::InboxSender;
use crate::state::{MutationBatch, StateStore};

use super::state::{
    BackgroundTaskStateAction, BackgroundTaskStateKey, BackgroundTaskStateSnapshot,
    PersistedTaskMeta,
};
use super::types::{
    TaskContext, TaskEvent, TaskId, TaskParentContext, TaskResult, TaskStatus, TaskSummary,
};

/// Errors from [`BackgroundTaskManager::send_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// No task with this ID exists.
    TaskNotFound,
    /// Caller's thread does not own the task.
    NotOwner,
    /// Task has already reached a terminal state.
    TaskTerminated(TaskStatus),
    /// Task is not a sub-agent (has no inbox).
    NoInbox,
    /// Inbox receiver was dropped (sub-agent ended).
    InboxClosed,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskNotFound => write!(f, "task not found"),
            Self::NotOwner => write!(f, "caller does not own this task"),
            Self::TaskTerminated(s) => write!(f, "task already {}", s.as_str()),
            Self::NoInbox => write!(f, "task has no inbox (not a sub-agent)"),
            Self::InboxClosed => write!(f, "sub-agent inbox closed"),
        }
    }
}

impl std::error::Error for SendError {}

/// Reserved names that cannot be used as task names.
const RESERVED_NAMES: &[&str] = &["parent", "self", "all", "broadcast"];

/// Errors from task spawn operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// The name is reserved by the system.
    ReservedName(String),
    /// Another running task on this thread already has this name.
    DuplicateName(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservedName(n) => write!(f, "'{n}' is a reserved name"),
            Self::DuplicateName(n) => write!(f, "a running task named '{n}' already exists"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Runtime-only handle for a live background task.
///
/// Contains only non-serializable runtime handles (cancel, join, inbox).
/// All metadata (status, error, result, timestamps) lives in the StateStore
/// under [`BackgroundTaskStateKey`].
struct TaskHandle {
    task_id: TaskId,
    owner_thread_id: String,
    cancel_handle: CancellationHandle,
    _join_handle: JoinHandle<()>,
    /// Inbox sender for sub-agent tasks (allows parent to send messages).
    agent_inbox: Option<InboxSender>,
}

/// Thread-scoped handle table for background tasks.
///
/// Spawns, tracks, cancels, and queries background tasks.
/// Task metadata (status, error, result, timestamps) is stored in the
/// [`StateStore`] as the single source of truth. This struct only holds
/// runtime handles (cancel, join, inbox).
pub struct BackgroundTaskManager {
    handles: Mutex<HashMap<TaskId, TaskHandle>>,
    counter: AtomicU64,
    owner_inbox: std::sync::RwLock<Option<InboxSender>>,
    store: std::sync::OnceLock<StateStore>,
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            owner_inbox: std::sync::RwLock::new(None),
            store: std::sync::OnceLock::new(),
        }
    }

    /// Set the inbox sender that background tasks receive for pushing
    /// messages to the owner thread.
    pub fn set_owner_inbox(&self, inbox: InboxSender) {
        *self.owner_inbox.write().expect("owner_inbox poisoned") = Some(inbox);
    }

    /// Provide the state store for metadata persistence.
    ///
    /// Called once during plugin registration or run start. Subsequent
    /// calls are silently ignored (OnceLock semantics).
    pub fn set_store(&self, store: StateStore) {
        let _ = self.store.set(store);
    }

    /// Validate a task name: check reserved names and uniqueness.
    fn validate_name(&self, name: &str, owner_thread_id: &str) -> Result<(), SpawnError> {
        if RESERVED_NAMES.contains(&name) {
            return Err(SpawnError::ReservedName(name.to_string()));
        }
        // Check uniqueness among running tasks on this thread
        if let Some(store) = self.store()
            && let Some(snap) = store.read::<BackgroundTaskStateKey>()
        {
            for meta in snap.tasks.values() {
                if meta.owner_thread_id == owner_thread_id
                    && !meta.status.is_terminal()
                    && meta.name.as_deref() == Some(name)
                {
                    return Err(SpawnError::DuplicateName(name.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Returns a reference to the store, if set.
    fn store(&self) -> Option<&StateStore> {
        self.store.get()
    }

    fn owner_inbox(&self) -> Option<InboxSender> {
        self.owner_inbox
            .read()
            .expect("owner_inbox poisoned")
            .clone()
    }

    fn next_task_id(&self) -> TaskId {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("bg_{n}")
    }

    /// Commit a state update to the store (if available).
    fn commit_meta(&self, action: BackgroundTaskStateAction) {
        if let Some(store) = self.store() {
            let mut batch = MutationBatch::new();
            batch.update::<BackgroundTaskStateKey>(action);
            // Ignore commit errors (e.g. key not registered in test setups)
            let _ = store.commit(batch);
        }
    }

    /// Spawn a background task.
    ///
    /// The `task_fn` receives a [`TaskContext`] and returns a `TaskResult`.
    /// Spawn a background task.
    ///
    /// `name` is an optional short identifier for addressing (e.g. "researcher").
    /// If provided, it must be unique among running tasks on this thread and
    /// must not be a reserved name ("parent", "self", "all", "broadcast").
    pub async fn spawn<F, Fut>(
        self: &Arc<Self>,
        owner_thread_id: &str,
        task_type: &str,
        name: Option<&str>,
        description: &str,
        parent_context: TaskParentContext,
        task_fn: F,
    ) -> Result<TaskId, SpawnError>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = TaskResult> + Send + 'static,
    {
        if let Some(n) = name {
            self.validate_name(n, owner_thread_id)?;
        }
        let task_id = self.next_task_id();
        let (cancel_handle, cancel_token) = CancellationToken::new_pair();
        let now = now_ms();

        let ctx = TaskContext {
            task_id: task_id.clone(),
            cancel_token,
            inbox: self.owner_inbox(),
        };

        let task_name = name.map(|n| n.to_string());

        // Commit initial metadata to the store
        self.commit_meta(BackgroundTaskStateAction::Upsert(Box::new(
            PersistedTaskMeta {
                task_id: task_id.clone(),
                owner_thread_id: owner_thread_id.to_string(),
                task_type: task_type.to_string(),
                name: task_name.clone(),
                description: description.to_string(),
                status: TaskStatus::Running,
                error: None,
                result: None,
                created_at_ms: now,
                completed_at_ms: None,
                parent_context: parent_context.clone(),
            },
        )));

        let manager = Arc::clone(self);
        let tid = task_id.clone();
        let owner_inbox = self.owner_inbox();
        let owner = owner_thread_id.to_string();
        let ttype = task_type.to_string();
        let tname = task_name.clone();
        let desc = description.to_string();

        let join_handle = tokio::spawn(async move {
            let result = task_fn(ctx).await;
            let completed_at = now_ms();

            // Update metadata in the store
            let (status, error, result_val) = match &result {
                TaskResult::Success(val) => (TaskStatus::Completed, None, Some(val.clone())),
                TaskResult::Failed(err) => (TaskStatus::Failed, Some(err.clone()), None),
                TaskResult::Cancelled => (TaskStatus::Cancelled, None, None),
            };

            manager.commit_meta(BackgroundTaskStateAction::Upsert(Box::new(
                PersistedTaskMeta {
                    task_id: tid.clone(),
                    owner_thread_id: owner,
                    task_type: ttype,
                    name: tname,
                    description: desc,
                    status,
                    error,
                    result: result_val,
                    created_at_ms: now,
                    completed_at_ms: Some(completed_at),
                    parent_context,
                },
            )));

            // Notify owner via inbox (terminal event).
            if let Some(ref inbox) = owner_inbox {
                let event = match &result {
                    TaskResult::Success(val) => TaskEvent::Completed {
                        task_id: tid.clone(),
                        result: Some(val.clone()),
                    },
                    TaskResult::Failed(err) => TaskEvent::Failed {
                        task_id: tid.clone(),
                        error: err.clone(),
                    },
                    TaskResult::Cancelled => TaskEvent::Cancelled {
                        task_id: tid.clone(),
                    },
                };
                inbox.send(serde_json::to_value(&event).unwrap_or_default());
            }
        });

        let handle = TaskHandle {
            task_id: task_id.clone(),
            owner_thread_id: owner_thread_id.to_string(),
            cancel_handle,
            _join_handle: join_handle,
            agent_inbox: None,
        };

        self.handles.lock().await.insert(task_id.clone(), handle);
        Ok(task_id)
    }

    /// Cancel a running task.
    pub async fn cancel(&self, task_id: &str) -> bool {
        let handles = self.handles.lock().await;
        if let Some(handle) = handles.get(task_id) {
            // Check status from the store
            if let Some(store) = self.store()
                && let Some(snap) = store.read::<BackgroundTaskStateKey>()
                && let Some(meta) = snap.tasks.get(task_id)
                && meta.status.is_terminal()
            {
                return false;
            }
            handle.cancel_handle.cancel();
            return true;
        }
        false
    }

    /// Cancel all running tasks for a given thread.
    /// Returns the number of tasks cancelled.
    pub async fn cancel_all(&self, owner_thread_id: &str) -> usize {
        let handles = self.handles.lock().await;
        let store_snap = self
            .store()
            .and_then(|s| s.read::<BackgroundTaskStateKey>());
        let mut count = 0;
        for handle in handles.values() {
            if handle.owner_thread_id != owner_thread_id {
                continue;
            }
            let is_terminal = store_snap
                .as_ref()
                .and_then(|snap| snap.tasks.get(&handle.task_id))
                .map(|m| m.status.is_terminal())
                .unwrap_or(false);
            if !is_terminal {
                handle.cancel_handle.cancel();
                count += 1;
            }
        }
        count
    }

    /// List all tasks for a given owner thread.
    pub async fn list(&self, owner_thread_id: &str) -> Vec<TaskSummary> {
        if let Some(store) = self.store()
            && let Some(snap) = store.read::<BackgroundTaskStateKey>()
        {
            return snap
                .tasks
                .values()
                .filter(|m| m.owner_thread_id == owner_thread_id)
                .map(Self::meta_to_summary)
                .collect();
        }
        Vec::new()
    }

    /// Get the summary of a specific task.
    pub async fn get(&self, task_id: &str) -> Option<TaskSummary> {
        self.store()
            .and_then(|s| s.read::<BackgroundTaskStateKey>())
            .and_then(|snap| snap.tasks.get(task_id).map(Self::meta_to_summary))
    }

    fn meta_to_summary(m: &PersistedTaskMeta) -> TaskSummary {
        TaskSummary {
            task_id: m.task_id.clone(),
            task_type: m.task_type.clone(),
            description: m.description.clone(),
            status: m.status,
            error: m.error.clone(),
            result: m.result.clone(),
            created_at_ms: m.created_at_ms,
            completed_at_ms: m.completed_at_ms,
            parent_context: m.parent_context.clone(),
        }
    }

    /// Restore persisted task metadata from a snapshot into the store.
    pub(crate) async fn restore_for_thread(
        &self,
        owner_thread_id: &str,
        snapshot: &BackgroundTaskStateSnapshot,
    ) {
        // First, write the snapshot data into the store
        if let Some(store) = self.store() {
            // Merge with existing data: only add tasks not already present
            let existing = store.read::<BackgroundTaskStateKey>().unwrap_or_default();

            for (task_id, meta) in &snapshot.tasks {
                if existing.tasks.contains_key(task_id) {
                    continue;
                }

                // Update counter
                if let Some(n) = task_id
                    .strip_prefix("bg_")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    self.counter
                        .fetch_max(n.saturating_add(1), Ordering::Relaxed);
                }

                let handles = self.handles.lock().await;
                let has_live_handle = handles.contains_key(task_id);
                drop(handles);

                let mut to_store = meta.clone();
                to_store.owner_thread_id = owner_thread_id.to_string();

                // Orphan detection
                if meta.status == TaskStatus::Running && !has_live_handle {
                    to_store.status = TaskStatus::Failed;
                    to_store.error =
                        Some("task orphaned: runtime restarted while running".to_string());
                }

                self.commit_meta(BackgroundTaskStateAction::Upsert(Box::new(to_store)));
            }
        }
    }

    /// Returns true if any task for the given thread is still running.
    pub async fn has_running(&self, owner_thread_id: &str) -> bool {
        if let Some(store) = self.store()
            && let Some(snap) = store.read::<BackgroundTaskStateKey>()
        {
            return snap
                .tasks
                .values()
                .any(|m| m.owner_thread_id == owner_thread_id && !m.status.is_terminal());
        }
        // Fallback: check handles if store not available
        self.handles
            .lock()
            .await
            .values()
            .any(|h| h.owner_thread_id == owner_thread_id)
    }

    /// Spawn a sub-agent as a background task with its own inbox.
    ///
    /// `name` is an optional short identifier for addressing via `send_message`.
    pub async fn spawn_agent<F, Fut>(
        self: &Arc<Self>,
        owner_thread_id: &str,
        name: Option<&str>,
        description: &str,
        parent_context: TaskParentContext,
        task_fn: F,
    ) -> Result<TaskId, SpawnError>
    where
        F: FnOnce(CancellationToken, InboxSender, crate::inbox::InboxReceiver) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = TaskResult> + Send + 'static,
    {
        if let Some(n) = name {
            self.validate_name(n, owner_thread_id)?;
        }
        let task_id = self.next_task_id();
        let (cancel_handle, cancel_token) = CancellationToken::new_pair();
        let now = now_ms();

        let (child_inbox_tx, child_inbox_rx) = crate::inbox::inbox_channel();
        let stored_sender = child_inbox_tx.clone();

        let task_name = name.map(|n| n.to_string());

        // Commit initial metadata
        self.commit_meta(BackgroundTaskStateAction::Upsert(Box::new(
            PersistedTaskMeta {
                task_id: task_id.clone(),
                owner_thread_id: owner_thread_id.to_string(),
                task_type: "sub_agent".to_string(),
                name: task_name.clone(),
                description: description.to_string(),
                status: TaskStatus::Running,
                error: None,
                result: None,
                created_at_ms: now,
                completed_at_ms: None,
                parent_context: parent_context.clone(),
            },
        )));

        let manager = Arc::clone(self);
        let tid = task_id.clone();
        let owner_inbox = self.owner_inbox();
        let owner = owner_thread_id.to_string();
        let tname = task_name.clone();
        let desc = description.to_string();

        let join_handle = tokio::spawn(async move {
            let result = task_fn(cancel_token, child_inbox_tx, child_inbox_rx).await;
            let completed_at = now_ms();

            let (status, error, result_val) = match &result {
                TaskResult::Success(val) => (TaskStatus::Completed, None, Some(val.clone())),
                TaskResult::Failed(err) => (TaskStatus::Failed, Some(err.clone()), None),
                TaskResult::Cancelled => (TaskStatus::Cancelled, None, None),
            };

            manager.commit_meta(BackgroundTaskStateAction::Upsert(Box::new(
                PersistedTaskMeta {
                    task_id: tid.clone(),
                    owner_thread_id: owner,
                    task_type: "sub_agent".to_string(),
                    name: tname,
                    description: desc,
                    status,
                    error,
                    result: result_val,
                    created_at_ms: now,
                    completed_at_ms: Some(completed_at),
                    parent_context,
                },
            )));

            let event = match &result {
                TaskResult::Success(val) => TaskEvent::Completed {
                    task_id: tid.clone(),
                    result: Some(val.clone()),
                },
                TaskResult::Failed(err) => TaskEvent::Failed {
                    task_id: tid.clone(),
                    error: err.clone(),
                },
                TaskResult::Cancelled => TaskEvent::Cancelled {
                    task_id: tid.clone(),
                },
            };
            if let Some(ref inbox) = owner_inbox {
                inbox.send(serde_json::to_value(&event).unwrap_or_default());
            }
        });

        let handle = TaskHandle {
            task_id: task_id.clone(),
            owner_thread_id: owner_thread_id.to_string(),
            cancel_handle,
            _join_handle: join_handle,
            agent_inbox: Some(stored_sender),
        };

        self.handles.lock().await.insert(task_id.clone(), handle);
        Ok(task_id)
    }

    /// Send a message to a child task's live inbox.
    ///
    /// This is the internal low-latency transport for parent→child
    /// communication within the same process. For cross-agent or durable
    /// messaging, use the mailbox-based `send_message` tool instead.
    pub async fn send_task_inbox_message(
        &self,
        task_id: &str,
        owner_thread_id: &str,
        sender_agent_id: &str,
        content: &str,
    ) -> Result<(), SendError> {
        let handles = self.handles.lock().await;
        let handle = handles.get(task_id).ok_or(SendError::TaskNotFound)?;

        // Authorization: sender must be on the same thread that owns the task
        if handle.owner_thread_id != owner_thread_id {
            return Err(SendError::NotOwner);
        }

        // Check status from the store
        if let Some(store) = self.store()
            && let Some(snap) = store.read::<BackgroundTaskStateKey>()
            && let Some(meta) = snap.tasks.get(task_id)
            && meta.status.is_terminal()
        {
            return Err(SendError::TaskTerminated(meta.status));
        }

        let inbox = handle.agent_inbox.as_ref().ok_or(SendError::NoInbox)?;

        let event = TaskEvent::Custom {
            task_id: task_id.to_string(),
            event_type: "agent_message".to_string(),
            payload: serde_json::json!({
                "from": sender_agent_id,
                "content": content,
            }),
        };

        if inbox.send(serde_json::to_value(&event).unwrap_or_default()) {
            Ok(())
        } else {
            Err(SendError::InboxClosed)
        }
    }

    #[cfg(test)]
    pub(crate) async fn persisted_snapshot(&self) -> HashMap<TaskId, PersistedTaskMeta> {
        if let Some(store) = self.store()
            && let Some(snap) = store.read::<BackgroundTaskStateKey>()
        {
            return snap.tasks;
        }
        HashMap::new()
    }
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

use awaken_contract::now_ms;
