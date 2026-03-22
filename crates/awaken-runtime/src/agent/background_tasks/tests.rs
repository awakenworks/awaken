use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::model::Phase;

use crate::plugins::Plugin;
use crate::runtime::{ExecutionEnv, PhaseContext, PhaseRuntime};
use crate::state::StateStore;

use super::manager::BackgroundTaskManager;
use super::plugin::BackgroundTaskPlugin;
use super::state::{
    BackgroundTaskStateAction, BackgroundTaskStateKey, BackgroundTaskStateSnapshot,
    BackgroundTaskView, BackgroundTaskViewAction, PersistedTaskMeta, TaskViewEntry,
};
use super::types::{CancellationHandle, TaskResult, TaskStatus, TaskSummary};

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
