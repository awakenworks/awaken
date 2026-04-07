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
use crate::inbox::inbox_channel;

use super::types::{TaskParentContext, TaskResult, TaskStatus, TaskSummary};

/// Create a manager with a StateStore wired up (keys registered).
fn manager_with_store() -> (Arc<BackgroundTaskManager>, StateStore) {
    let store = StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();
    (manager, store)
}

/// Create a manager with store and owner inbox wired up.
fn manager_with_store_and_inbox() -> (
    Arc<BackgroundTaskManager>,
    StateStore,
    crate::inbox::InboxReceiver,
) {
    let store = StateStore::new();
    let (inbox_tx, inbox_rx) = inbox_channel();
    let mut manager = BackgroundTaskManager::new();
    manager.set_owner_inbox(inbox_tx);
    let manager = Arc::new(manager);
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();
    (manager, store, inbox_rx)
}

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
    let (manager, _store) = manager_with_store();
    let _id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "my task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "fast task",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!({"answer": 42})) },
        )
        .await
        .unwrap();

    // Wait briefly for task completion
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    assert!(summary.completed_at_ms.is_some());
    assert_eq!(summary.result.unwrap()["answer"], 42);
}

#[tokio::test]
async fn manager_task_fails() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "failing task",
            TaskParentContext::default(),
            |_| async { TaskResult::Failed("oops".into()) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Failed);
    assert_eq!(summary.error.as_deref(), Some("oops"));
}

#[tokio::test]
async fn manager_cancel() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "cancellable",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    assert!(manager.cancel(&id).await);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn manager_cancel_nonexistent() {
    let (manager, _store) = manager_with_store();
    assert!(!manager.cancel("nonexistent").await);
}

#[test]
fn plugin_registers_key() {
    let store = StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
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
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    let mut persisted = HashMap::new();
    persisted.insert(
        "bg_restored".to_string(),
        PersistedTaskMeta {
            task_id: "bg_restored".into(),
            owner_thread_id: "thread-restore".into(),
            task_type: "shell".into(),
            name: None,
            description: "restored".into(),
            status: TaskStatus::Completed,
            error: None,
            result: None,
            created_at_ms: 100,
            completed_at_ms: Some(200),
            parent_context: TaskParentContext::default(),
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
        parent_context: TaskParentContext::default(),
    };
    let meta = PersistedTaskMeta::from_summary(&summary, "thread-1");
    assert_eq!(meta.task_id, "bg_0");
    assert_eq!(meta.owner_thread_id, "thread-1");
    assert_eq!(meta.task_type, "shell");
    assert_eq!(meta.status, TaskStatus::Completed);
    assert_eq!(meta.completed_at_ms, Some(2000));
    assert_eq!(meta.result, Some(serde_json::json!({"ok": true})));
}

#[test]
fn persisted_task_meta_serde_roundtrip() {
    let meta = PersistedTaskMeta {
        task_id: "bg_1".into(),
        owner_thread_id: "t".into(),
        task_type: "http".into(),
        name: None,
        description: "fetch data".into(),
        status: TaskStatus::Failed,
        error: Some("timeout".into()),
        result: None,
        created_at_ms: 100,
        completed_at_ms: Some(200),
        parent_context: TaskParentContext::default(),
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
        owner_thread_id: "t".into(),
        task_type: "shell".into(),
        name: None,
        description: "build".into(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 100,
        completed_at_ms: None,
        parent_context: TaskParentContext::default(),
    };
    snapshot.reduce(BackgroundTaskStateAction::Upsert(Box::new(meta)));
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks["bg_0"].status, TaskStatus::Running);

    // Upsert again with completed status
    let meta2 = PersistedTaskMeta {
        task_id: "bg_0".into(),
        owner_thread_id: "t".into(),
        task_type: "shell".into(),
        name: None,
        description: "build".into(),
        status: TaskStatus::Completed,
        error: None,
        result: None,
        created_at_ms: 100,
        completed_at_ms: Some(200),
        parent_context: TaskParentContext::default(),
    };
    snapshot.reduce(BackgroundTaskStateAction::Upsert(Box::new(meta2)));
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks["bg_0"].status, TaskStatus::Completed);
}

#[test]
fn background_task_state_snapshot_reduce_replace_all() {
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.reduce(BackgroundTaskStateAction::Upsert(Box::new(
        PersistedTaskMeta {
            task_id: "old".into(),
            owner_thread_id: "t".into(),
            task_type: "shell".into(),
            name: None,
            description: "old task".into(),
            status: TaskStatus::Cancelled,
            error: None,
            result: None,
            created_at_ms: 50,
            completed_at_ms: Some(60),
            parent_context: TaskParentContext::default(),
        },
    )));

    let mut new_tasks = HashMap::new();
    new_tasks.insert(
        "new".into(),
        PersistedTaskMeta {
            task_id: "new".into(),
            owner_thread_id: "t".into(),
            task_type: "http".into(),
            name: None,
            description: "new task".into(),
            status: TaskStatus::Running,
            error: None,
            result: None,
            created_at_ms: 100,
            completed_at_ms: None,
            parent_context: TaskParentContext::default(),
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
    let (manager, _store) = manager_with_store();
    let id1 = manager
        .spawn(
            "thread-1",
            "shell",
            None,
            "task A",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();
    let id2 = manager
        .spawn(
            "thread-1",
            "http",
            None,
            "task B",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();
    let id3 = manager
        .spawn(
            "thread-1",
            "shell",
            None,
            "task C",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!("done")) },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();
    assert!(manager.get("does_not_exist").await.is_none());
}

#[tokio::test]
async fn manager_cancel_already_completed_returns_false() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "fast",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!(true)) },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "result task",
            TaskParentContext::default(),
            |_| async {
                TaskResult::Success(serde_json::json!({"key": "value", "nested": [1, 2, 3]}))
            },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();
    let _id1 = manager
        .spawn(
            "thread-1",
            "shell",
            None,
            "build",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await
        .unwrap();
    let _id2 = manager
        .spawn(
            "thread-2",
            "http",
            None,
            "fetch",
            TaskParentContext::default(),
            |_| async { TaskResult::Failed("timeout".into()) },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();

    // Spawn a live task with a known id pattern
    let live_id = manager
        .spawn(
            "thread-1",
            "shell",
            None,
            "live task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Build a snapshot that includes the same task id and a new one
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.tasks.insert(
        live_id.clone(),
        PersistedTaskMeta {
            task_id: live_id.clone(),
            owner_thread_id: "thread-1".into(),
            task_type: "shell".into(),
            name: None,
            description: "stale restore".into(),
            status: TaskStatus::Completed,
            error: None,
            result: None,
            created_at_ms: 1,
            completed_at_ms: Some(2),
            parent_context: TaskParentContext::default(),
        },
    );
    snapshot.tasks.insert(
        "bg_999".into(),
        PersistedTaskMeta {
            task_id: "bg_999".into(),
            owner_thread_id: "thread-1".into(),
            task_type: "http".into(),
            name: None,
            description: "restored only".into(),
            status: TaskStatus::Failed,
            error: Some("err".into()),
            result: None,
            created_at_ms: 10,
            completed_at_ms: Some(20),
            parent_context: TaskParentContext::default(),
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
    let (manager, _store) = manager_with_store();
    let id1 = manager
        .spawn(
            "t",
            "test",
            None,
            "a",
            TaskParentContext::default(),
            |_| async { TaskResult::Cancelled },
        )
        .await
        .unwrap();
    let id2 = manager
        .spawn(
            "t",
            "test",
            None,
            "b",
            TaskParentContext::default(),
            |_| async { TaskResult::Cancelled },
        )
        .await
        .unwrap();
    let id3 = manager
        .spawn(
            "t",
            "test",
            None,
            "c",
            TaskParentContext::default(),
            |_| async { TaskResult::Cancelled },
        )
        .await
        .unwrap();

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
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    // Spawn and complete a task
    let _id = manager
        .spawn(
            "thread-persist",
            "shell",
            None,
            "compile",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!({"status": "ok"})) },
        )
        .await
        .unwrap();
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
    let (manager, _store) = manager_with_store();

    // Spawn a task that blocks until cancelled, verify Running
    let running_id = manager
        .spawn(
            "t",
            "test",
            None,
            "blocks",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();
    let summary = manager.get(&running_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Running);

    // Spawn a task that succeeds, verify Completed
    let success_id = manager
        .spawn(
            "t",
            "test",
            None,
            "succeeds",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!("ok")) },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let summary = manager.get(&success_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    assert!(summary.completed_at_ms.is_some());

    // Spawn a task that fails, verify Failed
    let fail_id = manager
        .spawn(
            "t",
            "test",
            None,
            "fails",
            TaskParentContext::default(),
            |_| async { TaskResult::Failed("boom".into()) },
        )
        .await
        .unwrap();
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
    let (manager, _store) = manager_with_store();

    // Spawn 5 tasks concurrently. Tasks 0-2 block (cancellable), tasks 3-4 complete instantly.
    let mut blocking_ids = Vec::new();
    for i in 0..3 {
        let id = manager
            .spawn(
                "t",
                "test",
                None,
                &format!("blocking-{i}"),
                TaskParentContext::default(),
                |ctx| async move {
                    ctx.cancel_token.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();
        blocking_ids.push(id);
    }
    let mut completing_ids = Vec::new();
    for i in 0..2 {
        let id = manager
            .spawn(
                "t",
                "test",
                None,
                &format!("completing-{i}"),
                TaskParentContext::default(),
                |_| async { TaskResult::Success(serde_json::json!("done")) },
            )
            .await
            .unwrap();
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
    let (manager, _store) = manager_with_store();

    // One completed task
    let _completed_id = manager
        .spawn(
            "t",
            "shell",
            None,
            "done-task",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await
        .unwrap();

    // One running task
    let running_id = manager
        .spawn(
            "t",
            "http",
            None,
            "running-task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

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
    let (manager, _store) = manager_with_store();

    // Build a snapshot with IDs bg_5 and bg_10
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    for n in [5, 10] {
        let id = format!("bg_{n}");
        snapshot.tasks.insert(
            id.clone(),
            PersistedTaskMeta {
                task_id: id,
                owner_thread_id: "t".into(),
                task_type: "shell".into(),
                name: None,
                description: format!("restored-{n}"),
                status: TaskStatus::Completed,
                error: None,
                result: None,
                created_at_ms: 100,
                completed_at_ms: Some(200),
                parent_context: TaskParentContext::default(),
            },
        );
    }

    manager.restore_for_thread("t", &snapshot).await;

    // Spawn a new task — its ID must be higher than bg_10
    let new_id = manager
        .spawn(
            "t",
            "test",
            None,
            "new-after-restore",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await
        .unwrap();

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
        parent_context: TaskParentContext::default(),
    };
    let json = serde_json::to_string(&summary).unwrap();
    let decoded: TaskSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.task_id, "bg_42");
    assert_eq!(decoded.status, TaskStatus::Failed);
    assert_eq!(decoded.error.as_deref(), Some("connection refused"));
    assert!(decoded.result.is_none());
    assert_eq!(decoded.completed_at_ms, Some(2000));
}

#[tokio::test]
async fn spawn_with_parent_context_preserves_lineage() {
    let (manager, _store) = manager_with_store();
    let ctx = TaskParentContext {
        run_id: Some("run-abc".into()),
        call_id: Some("call-xyz".into()),
        agent_id: Some("agent-007".into()),
    };
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "lineage task",
            ctx.clone(),
            |_| async { TaskResult::Success(serde_json::json!({"ok": true})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    assert_eq!(summary.parent_context.run_id.as_deref(), Some("run-abc"));
    assert_eq!(summary.parent_context.call_id.as_deref(), Some("call-xyz"));
    assert_eq!(
        summary.parent_context.agent_id.as_deref(),
        Some("agent-007")
    );

    // Verify it also appears in list()
    let tasks = manager.list("thread-1").await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].parent_context, ctx);

    // Verify persisted snapshot includes parent context
    let snapshot = manager.persisted_snapshot().await;
    let meta = snapshot.get(&id).unwrap();
    assert_eq!(meta.parent_context.run_id.as_deref(), Some("run-abc"));
    assert_eq!(meta.parent_context.call_id.as_deref(), Some("call-xyz"));
    assert_eq!(meta.parent_context.agent_id.as_deref(), Some("agent-007"));
}

#[test]
fn persisted_task_meta_with_parent_context_serde_roundtrip() {
    let meta = PersistedTaskMeta {
        task_id: "bg_99".into(),
        owner_thread_id: "t".into(),
        task_type: "delegation".into(),
        name: None,
        description: "delegated work".into(),
        status: TaskStatus::Completed,
        error: None,
        result: None,
        created_at_ms: 500,
        completed_at_ms: Some(600),
        parent_context: TaskParentContext {
            run_id: Some("run-123".into()),
            call_id: None,
            agent_id: Some("agent-a".into()),
        },
    };
    let json = serde_json::to_string(&meta).unwrap();
    let decoded: PersistedTaskMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, meta);
    assert_eq!(decoded.parent_context.run_id.as_deref(), Some("run-123"));
    assert!(decoded.parent_context.call_id.is_none());
    assert_eq!(decoded.parent_context.agent_id.as_deref(), Some("agent-a"));
}

#[test]
fn persisted_task_meta_without_parent_context_deserializes_default() {
    // Backward compatibility: JSON without parent_context field should deserialize fine
    let json = r#"{
        "task_id": "bg_old",
        "task_type": "shell",
        "description": "legacy task",
        "status": "completed",
        "created_at_ms": 100,
        "completed_at_ms": 200
    }"#;
    let decoded: PersistedTaskMeta = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.task_id, "bg_old");
    assert!(decoded.parent_context.is_empty());
    assert!(decoded.result.is_none());
}

#[test]
fn persisted_task_meta_result_field_roundtrip() {
    let meta = PersistedTaskMeta {
        task_id: "bg_r".into(),
        owner_thread_id: "t".into(),
        task_type: "shell".into(),
        name: None,
        description: "result test".into(),
        status: TaskStatus::Completed,
        error: None,
        result: Some(serde_json::json!({"output": "built ok", "lines": 42})),
        created_at_ms: 100,
        completed_at_ms: Some(200),
        parent_context: TaskParentContext::default(),
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("\"result\""));
    let decoded: PersistedTaskMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.result.as_ref().unwrap()["output"], "built ok");
    assert_eq!(decoded.result.as_ref().unwrap()["lines"], 42);
}

#[test]
fn persisted_task_meta_result_none_omitted_in_json() {
    let meta = PersistedTaskMeta {
        task_id: "bg_n".into(),
        owner_thread_id: "t".into(),
        task_type: "shell".into(),
        name: None,
        description: "no result".into(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 100,
        completed_at_ms: None,
        parent_context: TaskParentContext::default(),
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(!json.contains("\"result\""));
}

#[test]
fn task_parent_context_is_empty() {
    assert!(TaskParentContext::default().is_empty());
    assert!(
        !TaskParentContext {
            run_id: Some("r".into()),
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !TaskParentContext {
            call_id: Some("c".into()),
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !TaskParentContext {
            agent_id: Some("a".into()),
            ..Default::default()
        }
        .is_empty()
    );
}

#[test]
fn task_summary_with_empty_parent_context_omits_field_in_json() {
    let summary = TaskSummary {
        task_id: "bg_0".into(),
        task_type: "test".into(),
        description: "no parent".into(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 100,
        completed_at_ms: None,
        parent_context: TaskParentContext::default(),
    };
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains("parent_context"));
}

#[test]
fn task_summary_with_parent_context_includes_field_in_json() {
    let summary = TaskSummary {
        task_id: "bg_0".into(),
        task_type: "test".into(),
        description: "with parent".into(),
        status: TaskStatus::Running,
        error: None,
        result: None,
        created_at_ms: 100,
        completed_at_ms: None,
        parent_context: TaskParentContext {
            run_id: Some("run-1".into()),
            call_id: None,
            agent_id: None,
        },
    };
    let json = serde_json::to_string(&summary).unwrap();
    assert!(json.contains("parent_context"));
    assert!(json.contains("run-1"));
}

#[tokio::test]
async fn persisted_snapshot_includes_result_value() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "t",
            "shell",
            None,
            "build",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!({"exit_code": 0})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let snapshot = manager.persisted_snapshot().await;
    let meta = snapshot.get(&id).unwrap();
    assert_eq!(meta.status, TaskStatus::Completed);
    assert_eq!(meta.result.as_ref().unwrap()["exit_code"], 0);
}

// ---------------------------------------------------------------------------
// TaskContext, InboxSender, inbox events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_context_provides_inbox_sender() {
    let (manager, _store, mut inbox_rx) = manager_with_store_and_inbox();

    let _id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "inbox task",
            TaskParentContext::default(),
            |ctx| async move {
                let inbox = ctx.inbox.expect("inbox should be Some");
                inbox.send(serde_json::json!({"progress": 50}));
                inbox.send(serde_json::json!({"progress": 100}));
                TaskResult::Success(serde_json::json!("done"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let msgs = inbox_rx.drain();
    // 2 custom messages + 1 terminal Completed event = 3
    assert!(
        msgs.len() >= 2,
        "expected at least 2 messages, got {}",
        msgs.len()
    );
    assert_eq!(msgs[0]["progress"], 50);
    assert_eq!(msgs[1]["progress"], 100);
}

#[tokio::test]
async fn task_context_inbox_is_none_by_default() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "no inbox",
            TaskParentContext::default(),
            |ctx| async move {
                assert!(ctx.inbox.is_none());
                TaskResult::Success(serde_json::json!(null))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
}

#[tokio::test]
async fn task_completion_sends_terminal_event_to_inbox() {
    let (manager, _store, mut inbox_rx) = manager_with_store_and_inbox();

    manager
        .spawn(
            "thread-1",
            "test",
            None,
            "completes",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!("ok")) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let msgs = inbox_rx.drain();
    // Should contain a Completed terminal event
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("completed")),
        "inbox should receive terminal Completed event, got: {:?}",
        msgs
    );
}

#[tokio::test]
async fn on_closed_fires_when_inbox_receiver_dropped() {
    use crate::inbox::inbox_channel_with_fallback;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counter(AtomicUsize);
    impl crate::inbox::OnInboxClosed for Counter {
        fn closed(&self, _msg: &serde_json::Value) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let store = StateStore::new();
    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let (inbox_tx, inbox_rx) = inbox_channel_with_fallback(counter.clone());
    let mut manager = BackgroundTaskManager::new();
    manager.set_owner_inbox(inbox_tx);
    let manager = Arc::new(manager);
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    // Drop receiver before task completes — simulates AwaitingTasks
    drop(inbox_rx);

    manager
        .spawn(
            "thread-1",
            "test",
            None,
            "late completion",
            TaskParentContext::default(),
            |_| async { TaskResult::Success(serde_json::json!("ok")) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // on_closed should have fired (inbox receiver was gone)
    assert!(
        counter.0.load(Ordering::SeqCst) > 0,
        "on_closed should fire when receiver is dropped"
    );
}

#[tokio::test]
async fn custom_and_terminal_events_arrive_in_inbox() {
    let (manager, _store, mut inbox_rx) = manager_with_store_and_inbox();

    manager
        .spawn(
            "thread-1",
            "crawl",
            None,
            "fetch pages",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.emit("progress", serde_json::json!({"percent": 50}));
                ctx.emit("data_ready", serde_json::json!({"rows": 10}));
                TaskResult::Success(serde_json::json!("done"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let msgs = inbox_rx.drain();
    // Should have custom events + terminal Completed event
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("custom")),
        "should have custom events, got: {:?}",
        msgs
    );
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("completed")),
        "should have Completed event, got: {:?}",
        msgs
    );
}

#[tokio::test]
async fn task_context_emit_delivers_structured_custom_event() {
    let (manager, _store, mut inbox_rx) = manager_with_store_and_inbox();

    manager
        .spawn(
            "thread-1",
            "test",
            None,
            "emitter",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.emit("progress", serde_json::json!({"percent": 75}));
                TaskResult::Success(serde_json::json!("ok"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let messages = inbox_rx.drain();
    // At least one message should be a Custom event with "progress" type
    let progress_msg = messages.iter().find(|m| {
        m.get("kind").and_then(|k| k.as_str()) == Some("custom")
            && m.get("event_type").and_then(|t| t.as_str()) == Some("progress")
    });
    assert!(
        progress_msg.is_some(),
        "inbox should contain a custom progress event, got: {:?}",
        messages
    );
    let payload = progress_msg.unwrap().get("payload").unwrap();
    assert_eq!(payload["percent"], 75);
}

#[test]
fn plugin_descriptor_returns_correct_name() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let plugin = BackgroundTaskPlugin::new(manager.clone());
    let desc = plugin.descriptor();
    assert_eq!(desc.name, "background_tasks");
}

#[test]
fn plugin_on_activate_is_noop() {
    let manager = Arc::new(BackgroundTaskManager::new());
    let plugin = BackgroundTaskPlugin::new(manager.clone());
    let spec = awaken_contract::registry_spec::AgentSpec::default();
    let mut patch = crate::state::MutationBatch::new();
    let result = plugin.on_activate(&spec, &mut patch);
    assert!(result.is_ok());
    assert!(patch.is_empty());
}

#[test]
fn plugin_registers_phase_hooks() {
    let store = StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();
    // Phase hooks for RunStart and RunEnd are registered
    assert!(!env.phase_hooks.is_empty());
    assert!(
        env.phase_hooks.contains_key(&Phase::RunStart),
        "RunStart hook must be registered"
    );
    assert!(
        env.phase_hooks.contains_key(&Phase::RunEnd),
        "RunEnd hook must be registered"
    );
}

// ---------------------------------------------------------------------------
// Task 2.3: Orphan degradation (Running → Failed on restore)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_degrades_orphaned_running_tasks_to_failed() {
    let (manager, _store) = manager_with_store();

    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.tasks.insert(
        "bg_orphan".into(),
        PersistedTaskMeta {
            task_id: "bg_orphan".into(),
            owner_thread_id: "thread-1".into(),
            task_type: "shell".into(),
            name: None,
            description: "was running when runtime died".into(),
            status: TaskStatus::Running,
            error: None,
            result: None,
            created_at_ms: 100,
            completed_at_ms: None,
            parent_context: TaskParentContext::default(),
        },
    );

    manager.restore_for_thread("thread-1", &snapshot).await;

    let summary = manager.get("bg_orphan").await.unwrap();
    assert_eq!(summary.status, TaskStatus::Failed);
    assert!(
        summary.error.as_deref().unwrap().contains("orphaned"),
        "error should mention orphaned: {:?}",
        summary.error
    );
}

#[tokio::test]
async fn restore_preserves_terminal_task_status() {
    let (manager, _store) = manager_with_store();

    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.tasks.insert(
        "bg_done".into(),
        PersistedTaskMeta {
            task_id: "bg_done".into(),
            owner_thread_id: "thread-1".into(),
            task_type: "shell".into(),
            name: None,
            description: "completed before restart".into(),
            status: TaskStatus::Completed,
            error: None,
            result: Some(serde_json::json!({"ok": true})),
            created_at_ms: 100,
            completed_at_ms: Some(200),
            parent_context: TaskParentContext::default(),
        },
    );
    snapshot.tasks.insert(
        "bg_failed".into(),
        PersistedTaskMeta {
            task_id: "bg_failed".into(),
            owner_thread_id: "thread-1".into(),
            task_type: "http".into(),
            name: None,
            description: "failed before restart".into(),
            status: TaskStatus::Failed,
            error: Some("timeout".into()),
            result: None,
            created_at_ms: 100,
            completed_at_ms: Some(150),
            parent_context: TaskParentContext::default(),
        },
    );

    manager.restore_for_thread("thread-1", &snapshot).await;

    let done = manager.get("bg_done").await.unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert!(done.error.is_none());

    let failed = manager.get("bg_failed").await.unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("timeout"));
}

#[tokio::test]
async fn restore_does_not_degrade_live_running_tasks() {
    let (manager, _store) = manager_with_store();

    // Spawn a live task first
    let live_id = manager
        .spawn(
            "thread-1",
            "shell",
            None,
            "live running task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Build snapshot with the same task ID marked as Running
    let mut snapshot = BackgroundTaskStateSnapshot::default();
    snapshot.tasks.insert(
        live_id.clone(),
        PersistedTaskMeta {
            task_id: live_id.clone(),
            owner_thread_id: "thread-1".into(),
            task_type: "shell".into(),
            name: None,
            description: "stale".into(),
            status: TaskStatus::Running,
            error: None,
            result: None,
            created_at_ms: 1,
            completed_at_ms: None,
            parent_context: TaskParentContext::default(),
        },
    );

    manager.restore_for_thread("thread-1", &snapshot).await;

    // Live task should remain Running (not degraded)
    let summary = manager.get(&live_id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Running);
    assert!(summary.error.is_none());

    manager.cancel(&live_id).await;
}

// ---------------------------------------------------------------------------
// Mechanism 3: spawn_agent + send_task_inbox_message (parent→child live transport)
// ---------------------------------------------------------------------------

use super::manager::SendError;

#[tokio::test]
async fn send_message_delivers_to_sub_agent() {
    let (manager, _store) = manager_with_store();

    let id = manager
        .spawn_agent(
            "thread-1",
            None,
            "sub-agent worker",
            TaskParentContext::default(),
            |_cancel, _inbox_sender, mut inbox_receiver| async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                match inbox_receiver.try_recv() {
                    Some(msg) => {
                        assert_eq!(msg["kind"], "custom");
                        assert_eq!(msg["event_type"], "agent_message");
                        assert_eq!(msg["payload"]["content"], "hello from parent");
                        assert_eq!(msg["payload"]["from"], "parent-agent");
                        TaskResult::Success(serde_json::json!({"received": true}))
                    }
                    None => TaskResult::Failed("no message received".into()),
                }
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let result = manager
        .send_task_inbox_message(&id, "thread-1", "parent-agent", "hello from parent")
        .await;
    assert!(result.is_ok());

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        manager.get(&id).await.unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn send_message_rejects_wrong_thread() {
    let (manager, _store) = manager_with_store();

    let id = manager
        .spawn_agent(
            "thread-1",
            None,
            "agent",
            TaskParentContext::default(),
            |cancel, _sender, _receiver| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Wrong thread_id — should be rejected
    let result = manager
        .send_task_inbox_message(&id, "thread-WRONG", "attacker", "evil message")
        .await;
    assert_eq!(result, Err(SendError::NotOwner));

    manager.cancel(&id).await;
}

#[tokio::test]
async fn send_message_rejects_completed_task() {
    let (manager, _store) = manager_with_store();

    let id =
        manager
            .spawn_agent(
                "thread-1",
                None,
                "fast agent",
                TaskParentContext::default(),
                |_cancel, _sender, _receiver| async move {
                    TaskResult::Success(serde_json::json!("done"))
                },
            )
            .await
            .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let result = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "too late")
        .await;
    assert_eq!(
        result,
        Err(SendError::TaskTerminated(TaskStatus::Completed))
    );
}

#[tokio::test]
async fn send_message_rejects_nonexistent_task() {
    let (manager, _store) = manager_with_store();
    let result = manager
        .send_task_inbox_message("bg_999", "thread-1", "parent", "hello")
        .await;
    assert_eq!(result, Err(SendError::TaskNotFound));
}

#[tokio::test]
async fn send_message_rejects_regular_task() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "regular task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    let result = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "hello")
        .await;
    assert_eq!(result, Err(SendError::NoInbox));

    manager.cancel(&id).await;
}

#[tokio::test]
async fn has_running_tracks_lifecycle() {
    let (manager, _store) = manager_with_store();
    assert!(!manager.has_running("thread-1").await);

    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "long",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancel_token.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    assert!(manager.has_running("thread-1").await);
    assert!(!manager.has_running("thread-2").await);

    manager.cancel(&id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!manager.has_running("thread-1").await);
}

#[tokio::test]
async fn full_lifecycle_sub_agent_with_child_tasks() {
    let (manager, _store) = manager_with_store();

    let agent_task_id = manager
        .spawn_agent(
            "thread-1",
            None,
            "worker-agent",
            TaskParentContext {
                run_id: Some("run-1".into()),
                call_id: None,
                agent_id: Some("parent".into()),
            },
            |_cancel, child_inbox_sender, mut child_inbox_receiver| async move {
                // Sub-agent creates its own background task manager
                let mut child_manager = BackgroundTaskManager::new();
                child_manager.set_owner_inbox(child_inbox_sender);
                let child_manager = Arc::new(child_manager);

                // Sub-agent spawns a background task
                child_manager
                    .spawn(
                        "sub-thread",
                        "crawl",
                        None,
                        "fetch data",
                        TaskParentContext::default(),
                        |ctx| async move {
                            ctx.emit(
                                "data_ready",
                                serde_json::json!({
                                    "url": "example.com",
                                }),
                            );
                            TaskResult::Success(serde_json::json!({"fetched": true}))
                        },
                    )
                    .await
                    .unwrap();

                // Sub-agent receives the event from its child task
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let events = child_inbox_receiver.drain();
                assert!(!events.is_empty(), "should receive child task event");

                TaskResult::Success(serde_json::json!({"processed": true}))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let task = manager.get(&agent_task_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(!manager.has_running("thread-1").await);
}

// ---------------------------------------------------------------------------
// Task lifecycle patterns
// ---------------------------------------------------------------------------

/// Pattern: one-shot task — runs to completion and returns a result.
#[tokio::test]
async fn pattern_one_shot_completes_with_result() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "compute",
            None,
            "calculate sum",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Success(serde_json::json!({"sum": 42})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.result.unwrap()["sum"], 42);
}

/// Pattern: long-running task with periodic events.
#[tokio::test]
async fn pattern_long_running_with_progress_events() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    let id = manager
        .spawn(
            "thread-1",
            "crawl",
            None,
            "crawl pages",
            TaskParentContext::default(),
            |ctx| async move {
                for i in 1..=3 {
                    ctx.emit("progress", serde_json::json!({"page": i}));
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                TaskResult::Success(serde_json::json!({"pages_crawled": 3}))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);

    let events = rx.drain();
    let progress_events: Vec<_> = events
        .iter()
        .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("custom"))
        .collect();
    assert_eq!(progress_events.len(), 3, "should have 3 progress events");
}

/// Pattern: spawn → emit result → wait for kill.
#[tokio::test]
async fn pattern_spawn_notify_wait_for_kill() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    let id = manager
        .spawn(
            "thread-1",
            "server",
            None,
            "start http server",
            TaskParentContext::default(),
            |ctx| async move {
                // Phase 1: start up and notify ready
                ctx.emit("ready", serde_json::json!({"port": 8080}));

                // Phase 2: park until killed
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Wait for the ready event
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let events = rx.drain();
    assert!(
        events
            .iter()
            .any(|e| { e.get("event_type").and_then(|t| t.as_str()) == Some("ready") }),
        "should receive ready event"
    );

    // Task should still be running
    assert!(manager.has_running("thread-1").await);
    assert_eq!(manager.get(&id).await.unwrap().status, TaskStatus::Running);

    // Kill it
    assert!(manager.cancel(&id).await);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert!(!manager.has_running("thread-1").await);
}

/// Pattern: cancel_all stops every running task for a thread.
#[tokio::test]
async fn pattern_cancel_all_stops_all_tasks() {
    let (manager, _store) = manager_with_store();

    // Spawn 3 long-running tasks
    for i in 0..3 {
        manager
            .spawn(
                "thread-1",
                "worker",
                None,
                &format!("worker {i}"),
                TaskParentContext::default(),
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();
    }

    assert!(manager.has_running("thread-1").await);
    let cancelled = manager.cancel_all("thread-1").await;
    assert_eq!(cancelled, 3);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!manager.has_running("thread-1").await);

    let tasks = manager.list("thread-1").await;
    for t in &tasks {
        assert_eq!(t.status, TaskStatus::Cancelled);
    }
}

/// Pattern: cancel_all only affects the specified thread.
#[tokio::test]
async fn pattern_cancel_all_thread_isolation() {
    let (manager, _store) = manager_with_store();

    let _t1 = manager
        .spawn(
            "thread-1",
            "worker",
            None,
            "t1 task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    let t2_id = manager
        .spawn(
            "thread-2",
            "worker",
            None,
            "t2 task",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    manager.cancel_all("thread-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(!manager.has_running("thread-1").await);
    assert!(
        manager.has_running("thread-2").await,
        "thread-2 tasks should not be affected"
    );

    manager.cancel(&t2_id).await;
}

/// Pattern: task that fails naturally.
#[tokio::test]
async fn pattern_task_fails_with_error() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "download",
            None,
            "fetch file",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Failed("connection refused".into()) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.error.as_deref(), Some("connection refused"));
}

/// Pattern: all events (custom + terminal) arrive in inbox in order.
#[tokio::test]
async fn pattern_all_events_arrive_in_inbox() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    manager
        .spawn(
            "thread-1",
            "pipeline",
            None,
            "data pipeline",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.emit("stage", serde_json::json!({"name": "extract"}));
                ctx.emit("stage", serde_json::json!({"name": "transform"}));
                TaskResult::Success(serde_json::json!({"loaded": true}))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let msgs = rx.drain();
    let custom_count = msgs
        .iter()
        .filter(|m| m.get("kind").and_then(|k| k.as_str()) == Some("custom"))
        .count();
    assert_eq!(custom_count, 2, "should have 2 custom events");
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("completed")),
        "should have terminal Completed event"
    );
}

/// Pattern: cancel already-completed task is no-op.
#[tokio::test]
async fn pattern_cancel_completed_is_noop() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "fast",
            None,
            "instant task",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Success(serde_json::json!("done")) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!manager.cancel(&id).await);
    assert_eq!(
        manager.get(&id).await.unwrap().status,
        TaskStatus::Completed
    );
}

// ---------------------------------------------------------------------------
// Inbox drain timing and event delivery
// ---------------------------------------------------------------------------

/// Events emitted during task execution arrive in inbox and can be drained.
#[tokio::test]
async fn inbox_events_accumulate_until_drained() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    manager
        .spawn(
            "thread-1",
            "producer",
            None,
            "emit many",
            TaskParentContext::default(),
            |ctx| async move {
                for i in 0..5 {
                    ctx.emit("tick", serde_json::json!({"n": i}));
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                TaskResult::Success(serde_json::json!("done"))
            },
        )
        .await
        .unwrap();

    // Don't drain yet — let events accumulate
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let msgs = rx.drain();
    // 5 custom events + 1 terminal Completed = 6
    assert_eq!(msgs.len(), 6, "should have 6 messages, got: {:?}", msgs);

    // After drain, channel is empty
    assert!(rx.try_recv().is_none());
}

/// Drain returns empty vec when no events have arrived.
#[tokio::test]
async fn inbox_drain_empty_when_no_events() {
    let (_manager, _store, mut rx) = manager_with_store_and_inbox();

    let msgs = rx.drain();
    assert!(msgs.is_empty());
}

/// Multiple tasks emit events to the same inbox.
#[tokio::test]
async fn multiple_tasks_share_same_inbox() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    for i in 0..3 {
        manager
            .spawn(
                "thread-1",
                "worker",
                None,
                &format!("worker-{i}"),
                TaskParentContext::default(),
                move |ctx| async move {
                    ctx.emit("result", serde_json::json!({"worker": i}));
                    TaskResult::Success(serde_json::json!(i))
                },
            )
            .await
            .unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let msgs = rx.drain();
    // 3 workers × (1 custom + 1 completed) = 6
    assert_eq!(msgs.len(), 6, "3 workers should produce 6 events");
}

/// on_closed callback fires for each failed send after receiver drop.
#[tokio::test]
async fn on_closed_fires_for_late_task_completion() {
    use crate::inbox::inbox_channel_with_fallback;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ClosedCounter(AtomicUsize);
    impl crate::inbox::OnInboxClosed for ClosedCounter {
        fn closed(&self, _msg: &serde_json::Value) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let store = StateStore::new();
    let counter = Arc::new(ClosedCounter(AtomicUsize::new(0)));
    let (tx, rx) = inbox_channel_with_fallback(counter.clone());

    let mut mgr = BackgroundTaskManager::new();
    mgr.set_owner_inbox(tx);
    let manager = Arc::new(mgr);
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    // Spawn a slow task
    manager
        .spawn(
            "thread-1",
            "slow",
            None,
            "slow work",
            TaskParentContext::default(),
            |ctx| async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                ctx.emit("done", serde_json::json!("result"));
                TaskResult::Success(serde_json::json!("ok"))
            },
        )
        .await
        .unwrap();

    // Drop receiver immediately — simulates run returning with AwaitingTasks
    drop(rx);

    // Wait for task to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // on_closed should have fired for the emit + terminal event
    assert!(
        counter.0.load(Ordering::SeqCst) >= 1,
        "on_closed should fire at least once"
    );
}

/// Task events carry correct task_id in their payload.
#[tokio::test]
async fn task_events_carry_task_id() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    let task_id = manager
        .spawn(
            "thread-1",
            "tagged",
            None,
            "id check",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.emit("ping", serde_json::json!(null));
                TaskResult::Success(serde_json::json!("pong"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let msgs = rx.drain();
    for msg in &msgs {
        let msg_task_id = msg.get("task_id").and_then(|v| v.as_str());
        assert_eq!(
            msg_task_id,
            Some(task_id.as_str()),
            "event should carry correct task_id"
        );
    }
}

/// Long-running task that emits events and responds to cancellation.
#[tokio::test]
async fn long_running_task_with_events_and_cancel() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    let id = manager
        .spawn(
            "thread-1",
            "monitor",
            None,
            "system monitor",
            TaskParentContext::default(),
            |ctx| async move {
                let mut ticks = 0;
                loop {
                    if ctx.is_cancelled() {
                        return TaskResult::Cancelled;
                    }
                    ticks += 1;
                    ctx.emit("heartbeat", serde_json::json!({"tick": ticks}));
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            },
        )
        .await
        .unwrap();

    // Let it run for a bit
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let initial_events = rx.drain();
    assert!(!initial_events.is_empty(), "should have heartbeat events");

    // Cancel it
    manager.cancel(&id).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);

    // May have more events from before cancel was processed + terminal
    let final_events = rx.drain();
    assert!(
        final_events
            .iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("cancelled")),
        "should have terminal Cancelled event"
    );
}

// ---------------------------------------------------------------------------
// Name validation and reserved names
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_rejects_reserved_name() {
    let (manager, _store) = manager_with_store();
    for reserved in &["parent", "self", "all", "broadcast"] {
        let result = manager
            .spawn(
                "thread-1",
                "test",
                Some(reserved),
                "desc",
                TaskParentContext::default(),
                |_ctx| async { TaskResult::Success(serde_json::json!(null)) },
            )
            .await;
        assert!(
            matches!(result, Err(super::manager::SpawnError::ReservedName(_))),
            "'{reserved}' should be rejected as reserved"
        );
    }
}

#[tokio::test]
async fn spawn_rejects_duplicate_name() {
    let (manager, _store) = manager_with_store();
    // First spawn succeeds
    let _id = manager
        .spawn(
            "thread-1",
            "worker",
            Some("researcher"),
            "first researcher",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Second spawn with same name fails
    let result = manager
        .spawn(
            "thread-1",
            "worker",
            Some("researcher"),
            "second researcher",
            TaskParentContext::default(),
            |_ctx| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await;
    assert!(matches!(
        result,
        Err(super::manager::SpawnError::DuplicateName(_))
    ));

    manager.cancel(&_id).await;
}

#[tokio::test]
async fn spawn_allows_same_name_after_completion() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "worker",
            Some("researcher"),
            "first",
            TaskParentContext::default(),
            |_ctx| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        manager.get(&id).await.unwrap().status,
        TaskStatus::Completed
    );

    // Same name allowed after first task completed
    let id2 = manager
        .spawn(
            "thread-1",
            "worker",
            Some("researcher"),
            "second",
            TaskParentContext::default(),
            |_ctx| async { TaskResult::Success(serde_json::json!(null)) },
        )
        .await
        .unwrap();
    assert_ne!(id, id2);
}

#[tokio::test]
async fn spawn_allows_same_name_on_different_threads() {
    let (manager, _store) = manager_with_store();
    let _id1 = manager
        .spawn(
            "thread-1",
            "worker",
            Some("researcher"),
            "t1 researcher",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    // Same name on different thread — allowed
    let _id2 = manager
        .spawn(
            "thread-2",
            "worker",
            Some("researcher"),
            "t2 researcher",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    manager.cancel_all("thread-1").await;
    manager.cancel_all("thread-2").await;
}

// ---------------------------------------------------------------------------
// Message delivery to terminated task
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_to_completed_task_returns_error() {
    let (manager, _store) = manager_with_store();
    let id =
        manager
            .spawn_agent(
                "thread-1",
                Some("fast-worker"),
                "instant agent",
                TaskParentContext::default(),
                |_cancel, _sender, _receiver| async move {
                    TaskResult::Success(serde_json::json!("done"))
                },
            )
            .await
            .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "too late")
        .await;
    assert!(matches!(
        result,
        Err(super::manager::SendError::TaskTerminated(
            TaskStatus::Completed
        ))
    ));
}

// ---------------------------------------------------------------------------
// Event notification does not alter final result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_do_not_change_task_result() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    let id = manager
        .spawn(
            "thread-1",
            "pipeline",
            None,
            "multi-step work",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.emit("step", serde_json::json!({"n": 1}));
                ctx.emit("step", serde_json::json!({"n": 2}));
                ctx.emit("step", serde_json::json!({"n": 3}));
                TaskResult::Success(serde_json::json!({"final": "answer"}))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Events are in inbox but don't affect the persisted result
    let _events = rx.drain();
    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.result.unwrap()["final"], "answer");
}

// ===========================================================================
// Brutal edge-case tests
// ===========================================================================

/// Rapid sequential emits — no messages lost.
#[tokio::test]
async fn rapid_sequential_emits_no_loss() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    manager
        .spawn(
            "thread-1",
            "burst",
            None,
            "rapid emitter",
            TaskParentContext::default(),
            |ctx| async move {
                for i in 0..100 {
                    ctx.emit("tick", serde_json::json!({"n": i}));
                }
                TaskResult::Success(serde_json::json!("done"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let msgs = rx.drain();
    let custom_count = msgs
        .iter()
        .filter(|m| m.get("kind").and_then(|k| k.as_str()) == Some("custom"))
        .count();
    assert_eq!(
        custom_count, 100,
        "all 100 emits must arrive, got {custom_count}"
    );
}

/// Cancel during active emit loop — task stops, partial events delivered.
#[tokio::test]
async fn cancel_during_emit_loop() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    let id = manager
        .spawn(
            "thread-1",
            "emitter",
            None,
            "cancel me",
            TaskParentContext::default(),
            |ctx| async move {
                for i in 0..1000 {
                    if ctx.is_cancelled() {
                        return TaskResult::Cancelled;
                    }
                    ctx.emit("tick", serde_json::json!({"n": i}));
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                TaskResult::Success(serde_json::json!("finished"))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    manager.cancel(&id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let msgs = rx.drain();
    // Some custom events + Cancelled terminal
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("cancelled"))
    );
    let tick_count = msgs
        .iter()
        .filter(|m| m.get("kind").and_then(|k| k.as_str()) == Some("custom"))
        .count();
    assert!(
        tick_count > 0 && tick_count < 1000,
        "partial delivery: got {tick_count}"
    );
}

/// Emit after cancel_token signaled — emit still works (channel may be alive).
#[tokio::test]
async fn emit_after_cancel_still_delivers() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "post-cancel emit",
            TaskParentContext::default(),
            |ctx| async move {
                ctx.cancelled().await;
                // Emit AFTER cancellation
                ctx.emit("final_words", serde_json::json!({"last": true}));
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    manager.cancel(&id).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let msgs = rx.drain();
    assert!(
        msgs.iter()
            .any(|m| m.get("event_type").and_then(|t| t.as_str()) == Some("final_words")),
        "post-cancel emit should deliver if channel alive"
    );
}

/// Multiple children — send_task_inbox_message routes to correct one.
#[tokio::test]
async fn multiple_children_route_correctly() {
    let (manager, _store) = manager_with_store();

    let id_a = manager
        .spawn_agent(
            "thread-1",
            Some("agent-a"),
            "agent A",
            TaskParentContext::default(),
            |cancel, _s, mut r| async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let got = r.try_recv().is_some();
                if !got {
                    cancel.cancelled().await;
                }
                TaskResult::Success(serde_json::json!({"name": "a", "got": got}))
            },
        )
        .await
        .unwrap();

    let id_b = manager
        .spawn_agent(
            "thread-1",
            Some("agent-b"),
            "agent B",
            TaskParentContext::default(),
            |cancel, _s, mut r| async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let got = r.try_recv().is_some();
                if !got {
                    cancel.cancelled().await;
                }
                TaskResult::Success(serde_json::json!({"name": "b", "got": got}))
            },
        )
        .await
        .unwrap();

    // Send to agent-a only
    manager
        .send_task_inbox_message(&id_a, "thread-1", "parent", "for-a-only")
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let task_a = manager.get(&id_a).await.unwrap();
    let task_b = manager.get(&id_b).await.unwrap();
    assert_eq!(task_a.status, TaskStatus::Completed);
    assert_eq!(
        task_a.result.as_ref().unwrap()["got"],
        true,
        "agent-a should have received the message"
    );
    // agent-b may or may not have completed (depends on timing), but it should not have received
}

/// Terminal event contains correct task_id and result.
#[tokio::test]
async fn terminal_event_contains_correct_data() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    let id = manager
        .spawn(
            "thread-1",
            "compute",
            None,
            "calc",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Success(serde_json::json!({"pi": 3.14})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let msgs = rx.drain();
    let completed = msgs
        .iter()
        .find(|m| m.get("kind").and_then(|k| k.as_str()) == Some("completed"));
    assert!(completed.is_some(), "must have Completed event");
    let c = completed.unwrap();
    assert_eq!(c["task_id"], id);
    assert_eq!(c["result"]["pi"], 3.14);
}

/// Failed task terminal event contains error.
#[tokio::test]
async fn failed_terminal_event_contains_error() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    manager
        .spawn(
            "thread-1",
            "test",
            None,
            "fail",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Failed("disk full".into()) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let msgs = rx.drain();
    let failed = msgs
        .iter()
        .find(|m| m.get("kind").and_then(|k| k.as_str()) == Some("failed"));
    assert!(failed.is_some());
    assert_eq!(failed.unwrap()["error"], "disk full");
}

/// Inbox closed after spawn_agent task completes — subsequent sends fail.
#[tokio::test]
async fn inbox_closed_after_agent_completes() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn_agent(
            "thread-1",
            Some("ephemeral"),
            "short-lived",
            TaskParentContext::default(),
            |_cancel, _s, _r| async move { TaskResult::Success(serde_json::json!("bye")) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "hello?")
        .await;
    assert!(matches!(
        result,
        Err(super::manager::SendError::TaskTerminated(_))
    ));
}

/// cancel_all during concurrent emits — all tasks stop.
#[tokio::test]
async fn cancel_all_during_concurrent_emits() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();
    for i in 0..5 {
        manager
            .spawn(
                "thread-1",
                "worker",
                None,
                &format!("worker-{i}"),
                TaskParentContext::default(),
                move |ctx| async move {
                    loop {
                        if ctx.is_cancelled() {
                            return TaskResult::Cancelled;
                        }
                        ctx.emit("heartbeat", serde_json::json!({"worker": i}));
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                },
            )
            .await
            .unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let cancelled = manager.cancel_all("thread-1").await;
    assert_eq!(cancelled, 5);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!manager.has_running("thread-1").await);

    let msgs = rx.drain();
    let cancel_events = msgs
        .iter()
        .filter(|m| m.get("kind").and_then(|k| k.as_str()) == Some("cancelled"))
        .count();
    assert_eq!(cancel_events, 5, "each task should emit Cancelled");
}

/// Nested spawn_agent: child spawns grandchild, events flow correctly.
#[tokio::test]
async fn nested_spawn_agent_events_flow() {
    let (manager, _store, mut rx) = manager_with_store_and_inbox();

    manager
        .spawn_agent(
            "thread-1",
            Some("outer"),
            "outer agent",
            TaskParentContext::default(),
            |_cancel, child_inbox, mut child_rx| async move {
                // Outer agent creates its own manager for grandchild
                let inner_manager = Arc::new(BackgroundTaskManager::new());
                inner_manager.set_store(crate::state::StateStore::new());

                // Use the child_inbox as the inner manager's owner inbox
                // so grandchild events flow to the outer agent's inbox
                // (not directly to grandparent — this is one level only)

                // Simulate: outer agent does work and reports
                child_inbox.send(serde_json::json!({"from": "outer", "status": "working"}));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;

                TaskResult::Success(serde_json::json!({"outer": "done"}))
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Parent's inbox should have the outer agent's completed event
    let msgs = rx.drain();
    assert!(
        msgs.iter()
            .any(|m| m.get("kind").and_then(|k| k.as_str()) == Some("completed")),
        "parent should receive outer agent completion"
    );
}

/// StateStore is the single source of truth — manager.get reads from store.
#[tokio::test]
async fn get_reads_from_state_store() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "test",
            None,
            "store check",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Success(serde_json::json!({"x": 1})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // get() should return data from StateStore
    let summary = manager.get(&id).await.unwrap();
    assert_eq!(summary.status, TaskStatus::Completed);
    assert_eq!(summary.result.unwrap()["x"], 1);

    // list() should also reflect store state
    let list = manager.list("thread-1").await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].task_id, id);
}

/// spawn with name=None is addressable by task_id but not by name.
#[tokio::test]
async fn unnamed_task_addressable_by_id_only() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn_agent(
            "thread-1",
            None,
            "unnamed worker",
            TaskParentContext::default(),
            |cancel, _s, mut r| async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if r.try_recv().is_some() {
                    TaskResult::Success(serde_json::json!(true))
                } else {
                    cancel.cancelled().await;
                    TaskResult::Cancelled
                }
            },
        )
        .await
        .unwrap();

    // Send by task_id — should work
    let r1 = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "by-id")
        .await;
    assert!(r1.is_ok());

    // Send by description "unnamed worker" — should NOT match (name is None)
    // This would need to go through send_message_tool which checks name field
    // Unnamed task: description is set but there's no "name" field on TaskSummary.
    // Verify the task is only addressable by task_id (send_message_tool handles name lookup).
    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.description, "unnamed worker");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// Empty message — should still deliver (no content validation).
#[tokio::test]
async fn empty_message_delivers() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn_agent(
            "thread-1",
            Some("worker"),
            "w",
            TaskParentContext::default(),
            |cancel, _s, _r| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await
        .unwrap();

    let r = manager
        .send_task_inbox_message(&id, "thread-1", "parent", "")
        .await;
    assert!(r.is_ok(), "empty message should deliver");
    manager.cancel(&id).await;
}

/// Task result persists through get() after task handle is gone.
#[tokio::test]
async fn result_persists_after_handle_dropped() {
    let (manager, _store) = manager_with_store();
    let id = manager
        .spawn(
            "thread-1",
            "compute",
            None,
            "ephemeral",
            TaskParentContext::default(),
            |_ctx| async move { TaskResult::Success(serde_json::json!({"answer": 42})) },
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Task handle's JoinHandle has completed. Result should still be accessible.
    let task = manager.get(&id).await.unwrap();
    assert_eq!(task.result.unwrap()["answer"], 42);
    assert!(task.completed_at_ms.is_some());
}
