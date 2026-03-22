//! Background task management for agent tools.
//!
//! Provides a system for spawning, tracking, cancelling, and querying
//! background tasks. Tasks are tracked in-memory and outlive individual runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::runtime::{PhaseContext, PhaseHook};
use crate::state::{KeyScope, MutationBatch, StateCommand, StateKey, StateKeyOptions};
use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;

/// Unique identifier for a background task.
pub type TaskId = String;

pub const BACKGROUND_TASKS_PLUGIN_ID: &str = "background_tasks";

// ---------------------------------------------------------------------------
// TaskStatus
// ---------------------------------------------------------------------------

/// Status of a background task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

// ---------------------------------------------------------------------------
// TaskResult
// ---------------------------------------------------------------------------

/// Result produced by a background task on completion.
#[derive(Debug, Clone)]
pub enum TaskResult {
    Success(serde_json::Value),
    Failed(String),
    Cancelled,
}

impl TaskResult {
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Success(_) => TaskStatus::Completed,
            Self::Failed(_) => TaskStatus::Failed,
            Self::Cancelled => TaskStatus::Cancelled,
        }
    }
}

// ---------------------------------------------------------------------------
// TaskSummary
// ---------------------------------------------------------------------------

/// Summary of a background task visible to tools and plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: TaskId,
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// BackgroundTaskState (StateKey)
// ---------------------------------------------------------------------------

/// Cached task view stored in the state store for prompt injection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundTaskView {
    pub tasks: HashMap<String, TaskViewEntry>,
}

/// Lightweight view of a single background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskViewEntry {
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
}

/// Action for the background task view state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackgroundTaskViewAction {
    Replace {
        tasks: HashMap<String, TaskViewEntry>,
    },
    Clear,
}

impl BackgroundTaskView {
    fn reduce(&mut self, action: BackgroundTaskViewAction) {
        match action {
            BackgroundTaskViewAction::Replace { tasks } => {
                self.tasks = tasks;
            }
            BackgroundTaskViewAction::Clear => {
                self.tasks.clear();
            }
        }
    }
}

/// State key for the cached background task view.
pub struct BackgroundTaskViewKey;

impl StateKey for BackgroundTaskViewKey {
    const KEY: &'static str = "background_tasks";
    type Value = BackgroundTaskView;
    type Update = BackgroundTaskViewAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// BackgroundTaskStateKey — persisted task metadata
// ---------------------------------------------------------------------------

/// Persisted metadata for a single background task.
///
/// Task payloads (the actual futures) are NOT persisted — only metadata
/// (id, name, status, error message, timestamps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTaskMeta {
    pub task_id: TaskId,
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

impl PersistedTaskMeta {
    /// Build from a [`TaskSummary`].
    pub fn from_summary(summary: &TaskSummary) -> Self {
        Self {
            task_id: summary.task_id.clone(),
            task_type: summary.task_type.clone(),
            description: summary.description.clone(),
            status: summary.status,
            error: summary.error.clone(),
            created_at_ms: summary.created_at_ms,
            completed_at_ms: summary.completed_at_ms,
        }
    }
}

/// Persisted state for all background tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundTaskStateSnapshot {
    pub tasks: HashMap<TaskId, PersistedTaskMeta>,
}

/// Actions applied to the persisted background task state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackgroundTaskStateAction {
    /// Upsert a single task's metadata.
    Upsert(PersistedTaskMeta),
    /// Replace the entire task map (used on restore/sync).
    ReplaceAll {
        tasks: HashMap<TaskId, PersistedTaskMeta>,
    },
}

impl BackgroundTaskStateSnapshot {
    fn reduce(&mut self, action: BackgroundTaskStateAction) {
        match action {
            BackgroundTaskStateAction::Upsert(meta) => {
                self.tasks.insert(meta.task_id.clone(), meta);
            }
            BackgroundTaskStateAction::ReplaceAll { tasks } => {
                self.tasks = tasks;
            }
        }
    }
}

/// State key for persisted background task metadata.
///
/// Scoped to `Thread` so it survives across runs. On task completion or
/// failure the manager writes a state update; on resume, the plugin
/// restores known task metadata from this key.
pub struct BackgroundTaskStateKey;

impl StateKey for BackgroundTaskStateKey {
    const KEY: &'static str = "background_task_state";
    type Value = BackgroundTaskStateSnapshot;
    type Update = BackgroundTaskStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// CancellationHandle
// ---------------------------------------------------------------------------

/// Handle for cancelling a running task.
#[derive(Clone)]
pub struct CancellationHandle {
    sender: watch::Sender<bool>,
}

impl CancellationHandle {
    fn new() -> (Self, CancellationToken) {
        let (tx, rx) = watch::channel(false);
        (Self { sender: tx }, CancellationToken { receiver: rx })
    }

    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

/// Token that a task checks for cancellation.
#[derive(Clone)]
pub struct CancellationToken {
    receiver: watch::Receiver<bool>,
}

impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn cancelled(&mut self) {
        while !*self.receiver.borrow() {
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BackgroundTaskManager
// ---------------------------------------------------------------------------

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
    counter: std::sync::atomic::AtomicU64,
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn next_task_id(&self) -> TaskId {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    async fn restore_for_thread(
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
                    .fetch_max(n.saturating_add(1), std::sync::atomic::Ordering::Relaxed);
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
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// BackgroundTaskPlugin
// ---------------------------------------------------------------------------

/// Phase hook that syncs background task metadata into the persisted state.
///
/// Registered for both `RunStart` (restore from persisted state) and
/// `RunEnd` (persist current task state).
struct BackgroundTaskSyncHook {
    manager: Arc<BackgroundTaskManager>,
}

#[async_trait::async_trait]
impl PhaseHook for BackgroundTaskSyncHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        match ctx.phase {
            Phase::RunStart => {
                let thread_id = &ctx.run_input.identity.thread_id;
                let snapshot = ctx
                    .state::<BackgroundTaskStateKey>()
                    .cloned()
                    .unwrap_or_default();
                self.manager.restore_for_thread(thread_id, &snapshot).await;
                Ok(StateCommand::new())
            }
            Phase::RunEnd => {
                let tasks = self.manager.tasks.lock().await;
                let persisted: HashMap<TaskId, PersistedTaskMeta> = tasks
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
                    .collect();
                drop(tasks);

                let mut cmd = StateCommand::new();
                cmd.update::<BackgroundTaskStateKey>(BackgroundTaskStateAction::ReplaceAll {
                    tasks: persisted,
                });
                Ok(cmd)
            }
            _ => Ok(StateCommand::new()),
        }
    }
}

/// Plugin that registers the background task view state key and
/// the persisted task metadata state key.
pub struct BackgroundTaskPlugin {
    manager: Arc<BackgroundTaskManager>,
}

impl BackgroundTaskPlugin {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self { manager }
    }
}

impl Plugin for BackgroundTaskPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: BACKGROUND_TASKS_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<BackgroundTaskViewKey>(StateKeyOptions::default())?;
        registrar.register_key::<BackgroundTaskStateKey>(StateKeyOptions {
            persistent: true,
            scope: KeyScope::Thread,
            ..StateKeyOptions::default()
        })?;

        // Sync task metadata into persisted state at run boundaries.
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::RunStart,
            BackgroundTaskSyncHook {
                manager: self.manager.clone(),
            },
        )?;
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::RunEnd,
            BackgroundTaskSyncHook {
                manager: self.manager.clone(),
            },
        )?;

        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ExecutionEnv, PhaseContext, PhaseRuntime};
    use crate::state::StateStore;
    use awaken_contract::contract::identity::RunIdentity;
    use awaken_contract::model::Phase;

    #[test]
    fn task_status_terminal_check() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn task_status_as_str() {
        assert_eq!(TaskStatus::Running.as_str(), "running");
        assert_eq!(TaskStatus::Completed.as_str(), "completed");
        assert_eq!(TaskStatus::Failed.as_str(), "failed");
        assert_eq!(TaskStatus::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn task_result_status() {
        assert_eq!(
            TaskResult::Success(serde_json::json!(null)).status(),
            TaskStatus::Completed
        );
        assert_eq!(
            TaskResult::Failed("err".into()).status(),
            TaskStatus::Failed
        );
        assert_eq!(TaskResult::Cancelled.status(), TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn manager_spawn_and_list() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let _id = manager
            .spawn("thread-1", "test", "my task", |mut cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        let tasks = manager.list("thread-1").await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_type, "test");
        assert_eq!(tasks[0].description, "my task");
        assert_eq!(tasks[0].status, TaskStatus::Running);

        // Other threads see nothing
        let tasks = manager.list("thread-2").await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn manager_task_completes() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let id = manager
            .spawn("thread-1", "test", "fast task", |_| async {
                TaskResult::Success(serde_json::json!({"answer": 42}))
            })
            .await;

        // Wait briefly for task completion
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = manager.get(&id).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Completed);
        assert!(summary.completed_at_ms.is_some());
        assert_eq!(summary.result.unwrap()["answer"], 42);
    }

    #[tokio::test]
    async fn manager_task_fails() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let id = manager
            .spawn("thread-1", "test", "failing task", |_| async {
                TaskResult::Failed("oops".into())
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = manager.get(&id).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Failed);
        assert_eq!(summary.error.as_deref(), Some("oops"));
    }

    #[tokio::test]
    async fn manager_cancel() {
        let manager = Arc::new(BackgroundTaskManager::new());
        let id = manager
            .spawn("thread-1", "test", "cancellable", |mut cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;

        assert!(manager.cancel(&id).await);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = manager.get(&id).await.unwrap();
        assert_eq!(summary.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn manager_cancel_nonexistent() {
        let manager = Arc::new(BackgroundTaskManager::new());
        assert!(!manager.cancel("nonexistent").await);
    }

    #[test]
    fn plugin_registers_key() {
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        store
            .install_plugin(BackgroundTaskPlugin::new(manager))
            .unwrap();
        let registry = store.registry.lock();
        assert!(registry.keys_by_name.contains_key("background_tasks"));
        assert!(registry.keys_by_name.contains_key("background_task_state"));
    }

    #[tokio::test]
    async fn run_start_restores_persisted_metadata_into_manager() {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store.clone()).unwrap();
        let manager = Arc::new(BackgroundTaskManager::new());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
        let env = ExecutionEnv::from_plugins(&[plugin]).unwrap();
        store.register_keys(&env.key_registrations).unwrap();

        let mut persisted = HashMap::new();
        persisted.insert(
            "bg_restored".to_string(),
            PersistedTaskMeta {
                task_id: "bg_restored".into(),
                task_type: "shell".into(),
                description: "restored".into(),
                status: TaskStatus::Completed,
                error: None,
                created_at_ms: 100,
                completed_at_ms: Some(200),
            },
        );
        let mut patch = store.begin_mutation();
        patch.update::<BackgroundTaskStateKey>(BackgroundTaskStateAction::ReplaceAll {
            tasks: persisted,
        });
        store.commit(patch).unwrap();

        let ctx = PhaseContext::new(Phase::RunStart, store.snapshot())
            .with_run_identity(RunIdentity::for_thread("thread-restore"));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        let restored = manager.list("thread-restore").await;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].task_id, "bg_restored");
        assert_eq!(restored[0].status, TaskStatus::Completed);
    }

    #[test]
    fn persisted_task_meta_from_summary() {
        let summary = TaskSummary {
            task_id: "bg_0".into(),
            task_type: "shell".into(),
            description: "build project".into(),
            status: TaskStatus::Completed,
            error: None,
            result: Some(serde_json::json!({"ok": true})),
            created_at_ms: 1000,
            completed_at_ms: Some(2000),
        };
        let meta = PersistedTaskMeta::from_summary(&summary);
        assert_eq!(meta.task_id, "bg_0");
        assert_eq!(meta.task_type, "shell");
        assert_eq!(meta.status, TaskStatus::Completed);
        assert_eq!(meta.completed_at_ms, Some(2000));
    }

    #[test]
    fn persisted_task_meta_serde_roundtrip() {
        let meta = PersistedTaskMeta {
            task_id: "bg_1".into(),
            task_type: "http".into(),
            description: "fetch data".into(),
            status: TaskStatus::Failed,
            error: Some("timeout".into()),
            created_at_ms: 100,
            completed_at_ms: Some(200),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: PersistedTaskMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn background_task_state_snapshot_reduce_upsert() {
        let mut snapshot = BackgroundTaskStateSnapshot::default();
        let meta = PersistedTaskMeta {
            task_id: "bg_0".into(),
            task_type: "shell".into(),
            description: "build".into(),
            status: TaskStatus::Running,
            error: None,
            created_at_ms: 100,
            completed_at_ms: None,
        };
        snapshot.reduce(BackgroundTaskStateAction::Upsert(meta));
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks["bg_0"].status, TaskStatus::Running);

        // Upsert again with completed status
        let meta2 = PersistedTaskMeta {
            task_id: "bg_0".into(),
            task_type: "shell".into(),
            description: "build".into(),
            status: TaskStatus::Completed,
            error: None,
            created_at_ms: 100,
            completed_at_ms: Some(200),
        };
        snapshot.reduce(BackgroundTaskStateAction::Upsert(meta2));
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks["bg_0"].status, TaskStatus::Completed);
    }

    #[test]
    fn background_task_state_snapshot_reduce_replace_all() {
        let mut snapshot = BackgroundTaskStateSnapshot::default();
        snapshot.reduce(BackgroundTaskStateAction::Upsert(PersistedTaskMeta {
            task_id: "old".into(),
            task_type: "shell".into(),
            description: "old task".into(),
            status: TaskStatus::Cancelled,
            error: None,
            created_at_ms: 50,
            completed_at_ms: Some(60),
        }));

        let mut new_tasks = HashMap::new();
        new_tasks.insert(
            "new".into(),
            PersistedTaskMeta {
                task_id: "new".into(),
                task_type: "http".into(),
                description: "new task".into(),
                status: TaskStatus::Running,
                error: None,
                created_at_ms: 100,
                completed_at_ms: None,
            },
        );
        snapshot.reduce(BackgroundTaskStateAction::ReplaceAll { tasks: new_tasks });
        assert_eq!(snapshot.tasks.len(), 1);
        assert!(!snapshot.tasks.contains_key("old"));
        assert!(snapshot.tasks.contains_key("new"));
    }

    #[test]
    fn background_task_view_reduce_replace() {
        let mut view = BackgroundTaskView::default();
        let mut tasks = HashMap::new();
        tasks.insert(
            "t1".into(),
            TaskViewEntry {
                task_type: "shell".into(),
                description: "build".into(),
                status: TaskStatus::Running,
            },
        );
        view.reduce(BackgroundTaskViewAction::Replace { tasks });
        assert_eq!(view.tasks.len(), 1);
        assert_eq!(view.tasks["t1"].task_type, "shell");
    }

    #[test]
    fn background_task_view_reduce_clear() {
        let mut view = BackgroundTaskView {
            tasks: {
                let mut m = HashMap::new();
                m.insert(
                    "t1".into(),
                    TaskViewEntry {
                        task_type: "shell".into(),
                        description: "build".into(),
                        status: TaskStatus::Running,
                    },
                );
                m
            },
        };
        view.reduce(BackgroundTaskViewAction::Clear);
        assert!(view.tasks.is_empty());
    }

    #[test]
    fn cancellation_token_check() {
        let (handle, token) = CancellationHandle::new();
        assert!(!token.is_cancelled());
        handle.cancel();
        assert!(token.is_cancelled());
    }
}
