use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::model::Phase;

use crate::hooks::PhaseContext;
use crate::phase::{ExecutionEnv, PhaseRuntime};
use crate::plugins::Plugin;
use crate::state::StateStore;

use super::manager::BackgroundTaskManager;
use super::plugin::BackgroundTaskPlugin;
use super::state::{
    BackgroundTaskStateAction, BackgroundTaskStateKey, BackgroundTaskStateSnapshot,
    BackgroundTaskView, BackgroundTaskViewAction, PersistedTaskMeta, TaskViewEntry,
};
use crate::cancellation::CancellationToken;

use super::types::{TaskResult, TaskStatus, TaskSummary};

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
        .spawn("thread-1", "test", "my task", |cancel| async move {
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
        .spawn("thread-1", "test", "cancellable", |cancel| async move {
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
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
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
    let (handle, token) = CancellationToken::new_pair();
    assert!(!token.is_cancelled());
    handle.cancel();
    assert!(token.is_cancelled());
}

// ---------------------------------------------------------------------------
// Additional background task tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manager_multiple_concurrent_tasks() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let id1 = manager
        .spawn("thread-1", "shell", "task A", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;
    let id2 = manager
        .spawn("thread-1", "http", "task B", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;
    let id3 = manager
        .spawn("thread-1", "shell", "task C", |_| async {
            TaskResult::Success(serde_json::json!("done"))
        })
        .await;

    // Wait for the instant task to finish
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let tasks = manager.list("thread-1").await;
    assert_eq!(tasks.len(), 3);

    // id1 and id2 are still running, id3 completed
    let s1 = manager.get(&id1).await.unwrap();
    assert_eq!(s1.status, TaskStatus::Running);
    let s2 = manager.get(&id2).await.unwrap();
    assert_eq!(s2.status, TaskStatus::Running);
    let s3 = manager.get(&id3).await.unwrap();
    assert_eq!(s3.status, TaskStatus::Completed);

    // Cancel remaining
    assert!(manager.cancel(&id1).await);
    assert!(manager.cancel(&id2).await);
}

#[tokio::test]
async fn manager_get_nonexistent_returns_none() {
    let manager = Arc::new(BackgroundTaskManager::new());
    assert!(manager.get("does_not_exist").await.is_none());
}

#[tokio::test]
async fn manager_cancel_already_completed_returns_false() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let id = manager
        .spawn("thread-1", "test", "fast", |_| async {
            TaskResult::Success(serde_json::json!(true))
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        manager.get(&id).await.unwrap().status,
        TaskStatus::Completed
    );

    // Cancelling a completed task returns false
    assert!(!manager.cancel(&id).await);
}

#[tokio::test]
async fn manager_task_result_retrieval_after_success() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let id = manager
        .spawn("thread-1", "test", "result task", |_| async {
            TaskResult::Success(serde_json::json!({"key": "value", "nested": [1, 2, 3]}))
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    let result = summary.result.unwrap();
    assert_eq!(result["key"], "value");
    assert_eq!(result["nested"][0], 1);
    assert_eq!(result["nested"][2], 3);
}

#[tokio::test]
async fn manager_persisted_snapshot_includes_all_tasks() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let _id1 = manager
        .spawn("thread-1", "shell", "build", |_| async {
            TaskResult::Success(serde_json::json!(null))
        })
        .await;
    let _id2 = manager
        .spawn("thread-2", "http", "fetch", |_| async {
            TaskResult::Failed("timeout".into())
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let snapshot = manager.persisted_snapshot().await;
    assert_eq!(snapshot.len(), 2);

    // Both threads' tasks appear in the global snapshot
    for meta in snapshot.values() {
        assert!(meta.status.is_terminal());
    }
}

#[tokio::test]
async fn manager_restore_skips_existing_live_tasks() {
    let manager = Arc::new(BackgroundTaskManager::new());

    // Spawn a live task with a known id pattern
    let live_id = manager
        .spawn("thread-1", "shell", "live task", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;

    // Build a snapshot that includes the same task id and a new one
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.tasks.insert(
        live_id.clone(),
        PersistedTaskMeta {
            task_id: live_id.clone(),
            task_type: "shell".into(),
            description: "stale restore".into(),
            status: TaskStatus::Completed,
            error: None,
            created_at_ms: 1,
            completed_at_ms: Some(2),
        },
    );
    snapshot.tasks.insert(
        "bg_999".into(),
        PersistedTaskMeta {
            task_id: "bg_999".into(),
            task_type: "http".into(),
            description: "restored only".into(),
            status: TaskStatus::Failed,
            error: Some("err".into()),
            created_at_ms: 10,
            completed_at_ms: Some(20),
        },
    );

    manager.restore_for_thread("thread-1", &snapshot).await;

    // Live task should keep its Running status (not overwritten)
    let live = manager.get(&live_id).await.unwrap();
    assert_eq!(live.status, TaskStatus::Running);
    assert_eq!(live.description, "live task");

    // The new restored task should be visible
    let restored = manager.get("bg_999").await.unwrap();
    assert_eq!(restored.status, TaskStatus::Failed);
    assert_eq!(restored.error.as_deref(), Some("err"));

    // Clean up
    manager.cancel(&live_id).await;
}

#[tokio::test]
async fn manager_task_ids_are_sequential() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let id1 = manager
        .spawn("t", "test", "a", |_| async { TaskResult::Cancelled })
        .await;
    let id2 = manager
        .spawn("t", "test", "b", |_| async { TaskResult::Cancelled })
        .await;
    let id3 = manager
        .spawn("t", "test", "c", |_| async { TaskResult::Cancelled })
        .await;

    // IDs should be bg_0, bg_1, bg_2
    assert_eq!(id1, "bg_0");
    assert_eq!(id2, "bg_1");
    assert_eq!(id3, "bg_2");
}

#[tokio::test]
async fn run_end_persists_task_state() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    let manager = Arc::new(BackgroundTaskManager::new());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    // Spawn and complete a task
    let _id = manager
        .spawn("thread-persist", "shell", "compile", |_| async {
            TaskResult::Success(serde_json::json!({"status": "ok"}))
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Run the RunEnd phase to persist
    let ctx = PhaseContext::new(Phase::RunEnd, store.snapshot())
        .with_run_identity(RunIdentity::for_thread("thread-persist"));
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    // Verify the persisted state was written
    let snap = store.snapshot();
    let bg_state = snap.get::<BackgroundTaskStateKey>().unwrap();
    assert!(!bg_state.tasks.is_empty());
    let meta = bg_state.tasks.values().next().unwrap();
    assert_eq!(meta.task_type, "shell");
    assert_eq!(meta.status, TaskStatus::Completed);
}

#[tokio::test]
async fn manager_task_status_transitions_correctly() {
    let manager = Arc::new(BackgroundTaskManager::new());

    // Spawn a task that blocks until cancelled, verify Running
    let running_id = manager
        .spawn("t", "test", "blocks", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;
    let summary = manager.get(&running_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Running);

    // Spawn a task that succeeds, verify Completed
    let success_id = manager
        .spawn("t", "test", "succeeds", |_| async {
            TaskResult::Success(serde_json::json!("ok"))
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let summary = manager.get(&success_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    assert!(summary.completed_at_ms.is_some());

    // Spawn a task that fails, verify Failed
    let fail_id = manager
        .spawn("t", "test", "fails", |_| async {
            TaskResult::Failed("boom".into())
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let summary = manager.get(&fail_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Failed);
    assert_eq!(summary.error.as_deref(), Some("boom"));
    assert!(summary.completed_at_ms.is_some());

    // Verify the first task is still Running
    let summary = manager.get(&running_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Running);

    // Clean up
    manager.cancel(&running_id).await;
}

#[tokio::test]
async fn manager_concurrent_spawn_and_cancel() {
    let manager = Arc::new(BackgroundTaskManager::new());

    // Spawn 5 tasks concurrently. Tasks 0-2 block (cancellable), tasks 3-4 complete instantly.
    let mut blocking_ids = Vec::new();
    for i in 0..3 {
        let id = manager
            .spawn("t", "test", &format!("blocking-{i}"), |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            })
            .await;
        blocking_ids.push(id);
    }
    let mut completing_ids = Vec::new();
    for i in 0..2 {
        let id = manager
            .spawn("t", "test", &format!("completing-{i}"), |_| async {
                TaskResult::Success(serde_json::json!("done"))
            })
            .await;
        completing_ids.push(id);
    }

    // Wait for the instant tasks to finish
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Cancel the 3 blocking tasks
    for id in &blocking_ids {
        assert!(manager.cancel(id).await);
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify the 2 completing tasks are Completed
    for id in &completing_ids {
        let s = manager.get(id).await.unwrap();
        assert_eq!(s.status, TaskStatus::Completed);
    }

    // Verify the 3 cancelled tasks are Cancelled
    for id in &blocking_ids {
        let s = manager.get(id).await.unwrap();
        assert_eq!(s.status, TaskStatus::Cancelled);
    }

    // Total tasks in list
    let all = manager.list("t").await;
    assert_eq!(all.len(), 5);
    assert_eq!(
        all.iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count(),
        2
    );
    assert_eq!(
        all.iter()
            .filter(|t| t.status == TaskStatus::Cancelled)
            .count(),
        3
    );
}

#[tokio::test]
async fn persisted_snapshot_excludes_running_tasks() {
    // Actually: per the implementation, persisted_snapshot includes ALL tasks
    // (running and terminal). This test verifies running tasks ARE included
    // with their current state for potential restoration.
    let manager = Arc::new(BackgroundTaskManager::new());

    // One completed task
    let _completed_id = manager
        .spawn("t", "shell", "done-task", |_| async {
            TaskResult::Success(serde_json::json!(null))
        })
        .await;

    // One running task
    let running_id = manager
        .spawn("t", "http", "running-task", |cancel| async move {
            cancel.cancelled().await;
            TaskResult::Cancelled
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let snapshot = manager.persisted_snapshot().await;
    assert_eq!(snapshot.len(), 2);

    // The running task is included with Running status
    let running_meta = snapshot.get(&running_id).unwrap();
    assert_eq!(running_meta.status, TaskStatus::Running);
    assert!(running_meta.completed_at_ms.is_none());

    // The completed task has terminal status
    let terminal_count = snapshot.values().filter(|m| m.status.is_terminal()).count();
    assert_eq!(terminal_count, 1);

    // Clean up
    manager.cancel(&running_id).await;
}

#[tokio::test]
async fn restore_updates_counter_correctly() {
    let manager = Arc::new(BackgroundTaskManager::new());

    // Build a snapshot with IDs bg_5 and bg_10
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    for n in [5, 10] {
        let id = format!("bg_{n}");
        snapshot.tasks.insert(
            id.clone(),
            PersistedTaskMeta {
                task_id: id,
                task_type: "shell".into(),
                description: format!("restored-{n}"),
                status: TaskStatus::Completed,
                error: None,
                created_at_ms: 100,
                completed_at_ms: Some(200),
            },
        );
    }

    manager.restore_for_thread("t", &snapshot).await;

    // Spawn a new task — its ID must be higher than bg_10
    let new_id = manager
        .spawn("t", "test", "new-after-restore", |_| async {
            TaskResult::Success(serde_json::json!(null))
        })
        .await;

    // The counter should have been bumped to at least 11
    assert_eq!(new_id, "bg_11");

    // Total tasks: 2 restored + 1 new = 3
    let all = manager.list("t").await;
    assert_eq!(all.len(), 3);
}

#[test]
fn task_summary_serde_roundtrip() {
    let summary = TaskSummary {
        task_id: "bg_42".into(),
        task_type: "http".into(),
        description: "fetch API data".into(),
        status: TaskStatus::Failed,
        error: Some("connection refused".into()),
        result: None,
        created_at_ms: 1000,
        completed_at_ms: Some(2000),
    };
    let json = serde_json::to_string(&summary).unwrap();
    let decoded: TaskSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.task_id, "bg_42");
    assert_eq!(decoded.status, TaskStatus::Failed);
    assert_eq!(decoded.error.as_deref(), Some("connection refused"));
    assert!(decoded.result.is_none());
    assert_eq!(decoded.completed_at_ms, Some(2000));
}
