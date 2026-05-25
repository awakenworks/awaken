use super::*;
use crate::PendingMessageStore;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message,
};
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, RunRecord, RunStore, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Barrier;
use tokio::time::{Duration, sleep};

fn make_run(run_id: &str, thread_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 100,
        started_at: None,
        finished_at: None,
        updated_at: 100,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

// ── validate_id ──

#[test]
fn validate_id_rejects_slash() {
    assert!(validate_id("a/b", "id").is_err());
}

#[test]
fn validate_id_rejects_backslash() {
    assert!(validate_id("a\\b", "id").is_err());
}

#[test]
fn validate_id_rejects_null_char() {
    assert!(validate_id("a\0b", "id").is_err());
}

#[test]
fn validate_id_rejects_dot_dot() {
    assert!(validate_id("a..b", "id").is_err());
}

#[test]
fn validate_id_rejects_empty() {
    assert!(validate_id("", "id").is_err());
    assert!(validate_id("  ", "id").is_err());
}

#[test]
fn validate_id_rejects_control_chars() {
    assert!(validate_id("a\tb", "id").is_err());
    assert!(validate_id("a\nb", "id").is_err());
}

#[test]
fn validate_id_accepts_valid() {
    assert!(validate_id("abc-123", "id").is_ok());
    assert!(validate_id("thread_001", "id").is_ok());
}

// ── atomic_write ──

#[tokio::test]
async fn atomic_write_creates_parent_dirs() {
    let td = TempDir::new().unwrap();
    let dir = td.path().join("deep").join("nested");
    atomic_write(&dir, "test.json", r#"{"ok": true}"#)
        .await
        .unwrap();
    assert!(dir.join("test.json").exists());
}

#[tokio::test]
async fn atomic_write_overwrites_existing() {
    let td = TempDir::new().unwrap();
    let dir = td.path().to_path_buf();
    atomic_write(&dir, "test.json", r#"{"v": 1}"#)
        .await
        .unwrap();
    atomic_write(&dir, "test.json", r#"{"v": 2}"#)
        .await
        .unwrap();
    let content = tokio::fs::read_to_string(dir.join("test.json"))
        .await
        .unwrap();
    assert!(content.contains("\"v\": 2"));
}

// ── Corrupted JSON handling ──

#[tokio::test]
async fn read_json_returns_error_for_corrupted_json() {
    let td = TempDir::new().unwrap();
    let path = td.path().join("bad.json");
    tokio::fs::write(&path, "not valid json{{{").await.unwrap();
    let result: Result<Option<Thread>, StorageError> = read_json(&path).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        StorageError::Serialization(_)
    ));
}

#[tokio::test]
async fn read_json_returns_none_for_missing_file() {
    let td = TempDir::new().unwrap();
    let path = td.path().join("nonexistent.json");
    let result: Result<Option<Thread>, StorageError> = read_json(&path).await;
    assert!(result.unwrap().is_none());
}

// ── FileStore::new ──

#[test]
fn file_store_new_does_not_create_dirs_eagerly() {
    let td = TempDir::new().unwrap();
    let path = td.path().join("store");
    let _store = FileStore::new(&path);
    // Dirs are NOT created at construction time
    assert!(!path.exists());
}

// ── ThreadStore ──

#[tokio::test]
async fn file_store_thread_save_load_delete() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, thread.id);

    store.delete_thread(&thread.id).await.unwrap();
    assert!(store.load_thread(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
async fn file_store_save_thread_normalizes_lineage() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let mut thread = Thread::with_id("t-normalized");
    thread.resource_id = Some(" resource-a ".to_string());
    thread.parent_thread_id = Some(" parent-1 ".to_string());

    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("t-normalized").await.unwrap().unwrap();
    assert_eq!(loaded.resource_id.as_deref(), Some("resource-a"));
    assert_eq!(loaded.parent_thread_id.as_deref(), Some("parent-1"));
}

#[tokio::test]
async fn file_store_thread_load_missing() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    assert!(store.load_thread("no-such").await.unwrap().is_none());
}

#[tokio::test]
async fn file_store_save_thread_validated_serializes_concurrent_cycle_updates() {
    let td = TempDir::new().unwrap();
    let store = Arc::new(FileStore::new(td.path()));
    store.save_thread(&Thread::with_id("a")).await.unwrap();
    store.save_thread(&Thread::with_id("b")).await.unwrap();

    let guard = store.hierarchy_lock.lock().await;
    let barrier = Arc::new(Barrier::new(3));
    let spawn_update = |thread_id: &'static str, parent_thread_id: &'static str| {
        let store = store.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            store
                .save_thread_validated(
                    &Thread::with_id(thread_id).with_parent_thread_id(parent_thread_id),
                )
                .await
        })
    };

    let left = spawn_update("a", "b");
    let right = spawn_update("b", "a");
    barrier.wait().await;
    tokio::task::yield_now().await;
    sleep(Duration::from_millis(20)).await;
    assert!(!left.is_finished());
    assert!(!right.is_finished());
    drop(guard);

    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert_ne!(left.is_ok(), right.is_ok());

    let a = store.load_thread("a").await.unwrap().unwrap();
    let b = store.load_thread("b").await.unwrap().unwrap();
    assert!(
        !(a.parent_thread_id.as_deref() == Some("b") && b.parent_thread_id.as_deref() == Some("a"))
    );
}

#[tokio::test]
async fn file_store_instances_share_hierarchy_lock_for_same_path() {
    let td = TempDir::new().unwrap();
    let canonical_path = td.path().join("store");
    let alias_anchor = td.path().join("alias");
    std::fs::create_dir_all(&alias_anchor).unwrap();
    let aliased_path = alias_anchor.join("..").join("store");
    let left_store = Arc::new(FileStore::new(&canonical_path));
    let right_store = Arc::new(FileStore::new(&aliased_path));
    left_store.save_thread(&Thread::with_id("a")).await.unwrap();
    left_store.save_thread(&Thread::with_id("b")).await.unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let spawn_update =
        |store: Arc<FileStore>, thread_id: &'static str, parent_thread_id: &'static str| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .save_thread_validated(
                        &Thread::with_id(thread_id).with_parent_thread_id(parent_thread_id),
                    )
                    .await
            })
        };

    let left = spawn_update(left_store.clone(), "a", "b");
    let right = spawn_update(right_store.clone(), "b", "a");
    barrier.wait().await;

    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert_ne!(left.is_ok(), right.is_ok());

    let a = left_store.load_thread("a").await.unwrap().unwrap();
    let b = right_store.load_thread("b").await.unwrap().unwrap();
    assert!(
        !(a.parent_thread_id.as_deref() == Some("b") && b.parent_thread_id.as_deref() == Some("a"))
    );
}

#[tokio::test]
async fn file_store_list_threads() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    for i in 0..3 {
        let mut t = Thread::new();
        t.id = format!("t-{i:02}");
        store.save_thread(&t).await.unwrap();
    }
    let ids = store.list_threads(0, 100).await.unwrap();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn file_store_messages_save_load_delete() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();

    let msgs = vec![Message::user("hello")];
    store.save_messages(&thread.id, &msgs).await.unwrap();

    let loaded = store.load_messages(&thread.id).await.unwrap().unwrap();
    assert_eq!(loaded.len(), 1);

    store.delete_messages(&thread.id).await.unwrap();
    assert!(store.load_messages(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
async fn file_store_committed_messages_use_record_projection() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let run = make_run("r-records", "t-records");

    let version = store
        .checkpoint_append(
            "t-records",
            &[Message::user("hello").with_id("m-1".to_string())],
            Some(0),
            &run,
        )
        .await
        .unwrap();

    assert_eq!(version, 1);
    assert!(store.message_record_path("t-records", 1).exists());
    assert!(!store.messages_path("t-records").exists());
    let records = store
        .load_message_records("t-records")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(records[0].seq, 1);
    assert_eq!(records[0].message.id.as_deref(), Some("m-1"));
}

#[tokio::test]
async fn file_store_pending_freeze_appends_records_and_clears_pending() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    store
        .append_pending_message_records(
            "t-freeze-records",
            &[Message::user("pending").with_id("pending-1".to_string())],
            mode,
        )
        .await
        .unwrap();

    let frozen = store
        .freeze_pending_message_records("t-freeze-records", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap();

    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen[0].seq, 1);
    assert!(store.message_record_path("t-freeze-records", 1).exists());
    assert!(!store.messages_path("t-freeze-records").exists());
    assert!(
        store
            .load_pending_message_records("t-freeze-records")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn file_store_pending_append_assigns_id_and_rejects_duplicates() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    let records = store
        .append_pending_message_records("t-pending-id", &[Message::user("first")], mode)
        .await
        .unwrap();
    let generated_id = records[0].pending_id.clone();
    assert_eq!(
        records[0].message.id.as_deref(),
        Some(generated_id.as_str())
    );

    let err = store
        .append_pending_message_records(
            "t-pending-id",
            &[Message::user("again").with_id(generated_id)],
            mode,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Validation(message) if message.contains("already exists")));
}

#[tokio::test]
async fn file_store_pending_reorder_stale_order_reports_version_conflict() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    store
        .append_pending_message_records(
            "t-pending-reorder-conflict",
            &[
                Message::user("first").with_id("pending-1".to_string()),
                Message::user("second").with_id("pending-2".to_string()),
            ],
            mode,
        )
        .await
        .unwrap();

    let err = store
        .reorder_pending_message_records("t-pending-reorder-conflict", &["pending-2".to_string()])
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 1,
            actual: 2
        }
    ));
}

#[tokio::test]
async fn file_store_delete_messages_missing_thread_returns_not_found() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let err = store.delete_messages("no-such").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));
}

// ── RunStore ──

#[tokio::test]
async fn file_store_run_create_load() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let run = make_run("r-1", "t-1");
    store.create_run(&run).await.unwrap();
    let loaded = store.load_run("r-1").await.unwrap().unwrap();
    assert_eq!(loaded.thread_id, "t-1");
}

#[tokio::test]
async fn file_store_run_create_duplicate_returns_already_exists() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let run = make_run("r-1", "t-1");
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
async fn file_store_run_latest() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let mut r1 = make_run("r-1", "t-1");
    r1.updated_at = 100;
    let mut r2 = make_run("r-2", "t-1");
    r2.updated_at = 200;
    store.create_run(&r1).await.unwrap();
    store.create_run(&r2).await.unwrap();

    let latest = store.latest_run("t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r-2");
}

// ── Checkpoint ──

#[tokio::test]
async fn file_store_checkpoint_saves_messages_and_run() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let msgs = vec![Message::user("cp")];
    let run = make_run("r-cp", "t-1");

    store.checkpoint("t-1", &msgs, &run).await.unwrap();

    let loaded_msgs = store.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 1);
    let loaded_run = store.load_run("r-cp").await.unwrap().unwrap();
    assert_eq!(loaded_run.thread_id, "t-1");
}

#[tokio::test]
async fn file_store_checkpoint_waits_for_hierarchy_lock() {
    let td = TempDir::new().unwrap();
    let store = Arc::new(FileStore::new(td.path()));
    let guard = store.hierarchy_lock.lock().await;
    let handle = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .checkpoint(
                    "t-locked",
                    &[Message::user("cp")],
                    &make_run("r-locked", "t-locked"),
                )
                .await
        })
    };

    tokio::task::yield_now().await;
    sleep(Duration::from_millis(20)).await;
    assert!(!handle.is_finished());
    drop(guard);

    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn file_store_delete_thread_with_strategy_waits_for_hierarchy_lock() {
    let td = TempDir::new().unwrap();
    let store = Arc::new(FileStore::new(td.path()));
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();

    let guard = store.hierarchy_lock.lock().await;
    let handle = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Detach)
                .await
        })
    };

    tokio::task::yield_now().await;
    sleep(Duration::from_millis(20)).await;
    assert!(!handle.is_finished());
    drop(guard);

    handle.await.unwrap().unwrap();
    assert!(store.load_thread("root").await.unwrap().is_none());
    assert_eq!(
        store
            .load_thread("child")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        None
    );
}

#[tokio::test]
async fn file_store_new_rolls_back_incomplete_checkpoint_journal() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let old_run = make_run("r-old", "t-rollback");
    store
        .checkpoint("t-rollback", &[Message::user("old")], &old_run)
        .await
        .unwrap();

    let new_run = make_run("r-new", "t-rollback");
    let mut new_thread = store.load_thread("t-rollback").await.unwrap().unwrap();
    new_thread.apply_run_projection(&new_run);
    let thread_payload = serde_json::to_string_pretty(&new_thread).unwrap();
    let messages_payload = serde_json::to_string_pretty(&[Message::user("new")]).unwrap();
    let run_payload = serde_json::to_string_pretty(&new_run).unwrap();

    let staged_thread = stage_write(&store.threads_dir(), "t-rollback.json", &thread_payload)
        .await
        .unwrap();
    let staged_messages = stage_write(&store.messages_dir(), "t-rollback.json", &messages_payload)
        .await
        .unwrap();
    let staged_run = stage_write(&store.runs_dir(), "r-new.json", &run_payload)
        .await
        .unwrap();
    let staged = [staged_thread, staged_messages, staged_run];
    let tx_id = "rollback-test";
    let journal = CheckpointJournal {
        writes: staged
            .iter()
            .map(|write| {
                let backup = checkpoint_backup_path(&write.target, tx_id);
                CheckpointJournalWrite {
                    target: rel_path(td.path(), &write.target).unwrap(),
                    tmp: Some(rel_path(td.path(), &write.tmp_path).unwrap()),
                    backup: rel_path(td.path(), &backup).unwrap(),
                    had_target: write.target.exists(),
                }
            })
            .collect(),
    };
    std::fs::write(
        checkpoint_marker_path(td.path()),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    // Simulate a crash after the thread file was replaced but before
    // messages/run files were committed.
    let thread_backup = join_rel(td.path(), &journal.writes[0].backup);
    std::fs::rename(&staged[0].target, &thread_backup).unwrap();
    std::fs::rename(&staged[0].tmp_path, &staged[0].target).unwrap();

    let recovered = FileStore::new(td.path());
    let thread = recovered.load_thread("t-rollback").await.unwrap().unwrap();
    assert_eq!(thread.latest_run_id.as_deref(), Some("r-old"));
    let messages = recovered
        .load_messages("t-rollback")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(messages[0].text(), "old");
    assert!(recovered.load_run("r-new").await.unwrap().is_none());
    assert!(!checkpoint_marker_path(td.path()).exists());
}

#[tokio::test]
async fn file_store_new_rolls_back_incomplete_hierarchy_delete_journal() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();
    store
        .save_messages("root", &[Message::user("root message")])
        .await
        .unwrap();

    let mut updated_child = store.load_thread("child").await.unwrap().unwrap();
    updated_child.parent_thread_id = None;
    updated_child.touch(2_000);
    let child_payload = serde_json::to_string_pretty(&updated_child).unwrap();
    let child_write = stage_write(&store.threads_dir(), "child.json", &child_payload)
        .await
        .unwrap();
    let root_thread_delete = stage_delete(store.thread_path("root")).unwrap();
    let root_messages_delete = stage_delete(store.messages_path("root")).unwrap();
    let ops = [
        StagedFileOp::Write(child_write.clone()),
        StagedFileOp::Delete(root_thread_delete.clone()),
        StagedFileOp::Delete(root_messages_delete.clone()),
    ];
    let tx_id = "delete-rollback-test";
    let journal = CheckpointJournal {
        writes: ops
            .iter()
            .map(|op| {
                let target = staged_op_target(op);
                let backup = checkpoint_backup_path(target, tx_id);
                CheckpointJournalWrite {
                    target: rel_path(td.path(), target).unwrap(),
                    tmp: staged_op_tmp(op).map(|tmp| rel_path(td.path(), tmp).unwrap()),
                    backup: rel_path(td.path(), &backup).unwrap(),
                    had_target: target.exists(),
                }
            })
            .collect(),
    };
    std::fs::write(
        checkpoint_marker_path(td.path()),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    // Simulate a crash after the child update and root thread delete were
    // committed, but before root messages were deleted.
    let child_backup = join_rel(td.path(), &journal.writes[0].backup);
    std::fs::rename(&child_write.target, &child_backup).unwrap();
    std::fs::rename(&child_write.tmp_path, &child_write.target).unwrap();
    let root_thread_backup = join_rel(td.path(), &journal.writes[1].backup);
    std::fs::rename(store.thread_path("root"), &root_thread_backup).unwrap();

    let recovered = FileStore::new(td.path());
    let root = recovered.load_thread("root").await.unwrap().unwrap();
    let child = recovered.load_thread("child").await.unwrap().unwrap();
    let messages = recovered.load_messages("root").await.unwrap().unwrap();

    assert_eq!(root.id, "root");
    assert_eq!(child.parent_thread_id.as_deref(), Some("root"));
    assert_eq!(messages[0].text(), "root message");
    assert!(!checkpoint_marker_path(td.path()).exists());
}

// ── Missing directory recovery ──

#[tokio::test]
async fn file_store_operations_create_dirs_on_demand() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path().join("fresh"));
    // This should work even though the dirs don't exist yet
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread(&thread.id).await.unwrap();
    assert!(loaded.is_some());
}

// ── validate_id edge cases for IDs used in operations ──

#[tokio::test]
async fn file_store_rejects_traversal_thread_id() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let err = store.load_thread("../escape").await.unwrap_err();
    assert!(matches!(err, StorageError::Io(_)));
}

#[tokio::test]
async fn file_store_rejects_slash_in_run_id() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let err = store.load_run("a/b").await.unwrap_err();
    assert!(matches!(err, StorageError::Io(_)));
}

// ── ProfileStore ──

#[tokio::test]
async fn profile_file_set_and_get() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let owner = ProfileOwner::Agent("alice".into());
    store
        .set(&owner, "lang", serde_json::json!("en"))
        .await
        .unwrap();
    let entry = ProfileStore::get(&store, &owner, "lang")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.key, "lang");
    assert_eq!(entry.value, serde_json::json!("en"));
    assert!(entry.updated_at > 0);
}

#[tokio::test]
async fn profile_file_get_missing() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let result = ProfileStore::get(&store, &ProfileOwner::System, "nonexistent")
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn profile_file_delete_and_clear() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let owner = ProfileOwner::Agent("bob".into());

    // Delete non-existent is fine
    ProfileStore::delete(&store, &owner, "missing")
        .await
        .unwrap();

    // Set, delete, verify gone
    store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
    ProfileStore::delete(&store, &owner, "k").await.unwrap();
    assert!(
        ProfileStore::get(&store, &owner, "k")
            .await
            .unwrap()
            .is_none()
    );

    // Clear owner
    store.set(&owner, "a", serde_json::json!(1)).await.unwrap();
    store.set(&owner, "b", serde_json::json!(2)).await.unwrap();
    store.clear_owner(&owner).await.unwrap();
    assert!(ProfileStore::list(&store, &owner).await.unwrap().is_empty());

    // Clear again is idempotent
    store.clear_owner(&owner).await.unwrap();
}

#[tokio::test]
async fn profile_file_list_sorted() {
    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());
    let alice = ProfileOwner::Agent("alice".into());
    let bob = ProfileOwner::Agent("bob".into());
    store
        .set(&alice, "z", serde_json::json!("last"))
        .await
        .unwrap();
    store
        .set(&alice, "a", serde_json::json!("first"))
        .await
        .unwrap();
    store
        .set(&bob, "x", serde_json::json!("other"))
        .await
        .unwrap();

    let entries = ProfileStore::list(&store, &alice).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, "a");
    assert_eq!(entries[1].key, "z");

    // Bob's entries are isolated
    assert_eq!(ProfileStore::list(&store, &bob).await.unwrap().len(), 1);
}

mod config;
