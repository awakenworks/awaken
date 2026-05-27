use super::*;
use crate::PendingMessageStore;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message, pending_queue_revision,
};
use awaken_contract::contract::storage::{
    RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use std::sync::Arc;
use tokio::sync::Barrier;

fn make_run(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
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
        status,
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

// ── ThreadStore ──

#[tokio::test]
async fn thread_save_and_load() {
    let store = InMemoryStore::new();
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, thread.id);
}

#[tokio::test]
async fn thread_load_missing_returns_none() {
    let store = InMemoryStore::new();
    assert!(store.load_thread("no-such").await.unwrap().is_none());
}

#[tokio::test]
async fn thread_delete_removes_thread_and_messages() {
    let store = InMemoryStore::new();
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();
    store
        .save_messages(&thread.id, &[Message::user("hello")])
        .await
        .unwrap();

    store.delete_thread(&thread.id).await.unwrap();
    assert!(store.load_thread(&thread.id).await.unwrap().is_none());
    assert!(store.load_messages(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
async fn thread_list_with_pagination() {
    let store = InMemoryStore::new();
    for i in 0..5 {
        let mut t = Thread::new();
        t.id = format!("t-{i:02}");
        store.save_thread(&t).await.unwrap();
    }
    let page = store.list_threads(1, 2).await.unwrap();
    assert_eq!(page.len(), 2);
}

#[tokio::test]
async fn save_thread_validated_serializes_concurrent_cycle_updates() {
    let store = Arc::new(InMemoryStore::new());
    store.save_thread(&Thread::with_id("a")).await.unwrap();
    store.save_thread(&Thread::with_id("b")).await.unwrap();

    let read_guard = store.threads.read().await;
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
    drop(read_guard);

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
async fn messages_save_and_load() {
    let store = InMemoryStore::new();
    let msgs = vec![Message::user("hi"), Message::assistant("hello")];
    store.save_messages("t-1", &msgs).await.unwrap();
    let loaded = store.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
}

#[tokio::test]
async fn messages_load_missing_returns_none() {
    let store = InMemoryStore::new();
    assert!(store.load_messages("no-such").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_messages_requires_existing_thread() {
    let store = InMemoryStore::new();
    let err = store.delete_messages("no-such").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));
}

#[tokio::test]
async fn delete_messages_for_existing_thread() {
    let store = InMemoryStore::new();
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();
    store
        .save_messages(&thread.id, &[Message::user("hi")])
        .await
        .unwrap();

    store.delete_messages(&thread.id).await.unwrap();
    assert!(store.load_messages(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
async fn pending_messages_append_edit_reorder_and_retract() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    let first = Message::user("first").with_id("pending-1".to_string());
    let second = Message::user("second").with_id("pending-2".to_string());

    let appended = store
        .append_pending_message_records("thread-pending", &[first, second], mode)
        .await
        .unwrap();
    assert_eq!(appended.len(), 2);
    assert_eq!(appended[0].position, 1);
    assert_eq!(appended[1].position, 2);

    let edited = store
        .update_pending_message_record(
            "thread-pending",
            "pending-1",
            Message::user("edited").with_id("pending-1".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(edited.message.text(), "edited");

    let reordered = store
        .reorder_pending_message_records(
            "thread-pending",
            &["pending-2".to_string(), "pending-1".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(reordered[0].pending_id, "pending-2");
    assert_eq!(reordered[0].position, 1);
    assert_eq!(reordered[1].position, 2);

    let retracted = store
        .retract_pending_message_record("thread-pending", "pending-2")
        .await
        .unwrap();
    assert_eq!(retracted.pending_id, "pending-2");
    let pending = store
        .load_pending_message_records("thread-pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, "pending-1");
    assert_eq!(pending[0].position, 1);
}

#[tokio::test]
async fn list_threads_with_pending_messages_reports_non_empty_threads() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    store
        .append_pending_message_records(
            "thread-a",
            &[Message::user("a").with_id("a1".to_string())],
            mode.clone(),
        )
        .await
        .unwrap();
    store
        .append_pending_message_records(
            "thread-b",
            &[Message::user("b").with_id("b1".to_string())],
            mode,
        )
        .await
        .unwrap();

    let ids = store
        .list_threads_with_pending_messages(0, None)
        .await
        .unwrap();
    assert_eq!(ids, vec!["thread-a".to_string(), "thread-b".to_string()]);

    // `limit` caps the result.
    assert_eq!(
        store
            .list_threads_with_pending_messages(1, None)
            .await
            .unwrap(),
        vec!["thread-a".to_string()]
    );

    // `after` cursor pages past ids already seen: a page of size 1 anchored at
    // "thread-a" yields the next id only.
    assert_eq!(
        store
            .list_threads_with_pending_messages(1, Some("thread-a"))
            .await
            .unwrap(),
        vec!["thread-b".to_string()]
    );

    // A thread with no remaining pending drops out of the scan.
    store
        .retract_pending_message_record("thread-a", "a1")
        .await
        .unwrap();
    assert_eq!(
        store
            .list_threads_with_pending_messages(0, None)
            .await
            .unwrap(),
        vec!["thread-b".to_string()]
    );
}

#[tokio::test]
async fn pending_mutations_reject_stale_revisions() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    let appended = store
        .append_pending_message_records(
            "thread-pending-cas",
            &[
                Message::user("first").with_id("pending-1".to_string()),
                Message::user("second").with_id("pending-2".to_string()),
            ],
            mode,
        )
        .await
        .unwrap();

    let queue_revision = pending_queue_revision(&appended);
    let stale_record_revision = appended[0].revision;
    let edited = store
        .update_pending_message_record_checked(
            "thread-pending-cas",
            "pending-1",
            Some(stale_record_revision),
            Message::user("edited").with_id("pending-1".to_string()),
        )
        .await
        .unwrap();
    assert!(edited.revision > stale_record_revision);

    let stale_update = store
        .update_pending_message_record_checked(
            "thread-pending-cas",
            "pending-1",
            Some(stale_record_revision),
            Message::user("stale").with_id("pending-1".to_string()),
        )
        .await
        .unwrap_err();
    assert!(matches!(stale_update, StorageError::VersionConflict { .. }));

    let stale_reorder = store
        .reorder_pending_message_records_checked(
            "thread-pending-cas",
            Some(queue_revision),
            &["pending-2".to_string(), "pending-1".to_string()],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        stale_reorder,
        StorageError::VersionConflict { .. }
    ));

    let stale_retract = store
        .retract_pending_message_record_checked(
            "thread-pending-cas",
            "pending-1",
            Some(stale_record_revision),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        stale_retract,
        StorageError::VersionConflict { .. }
    ));
}

#[tokio::test]
async fn pending_reorder_error_keeps_existing_order() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    store
        .append_pending_message_records(
            "thread-reorder-error",
            &[
                Message::user("first").with_id("pending-1".to_string()),
                Message::user("second").with_id("pending-2".to_string()),
            ],
            mode,
        )
        .await
        .unwrap();

    let err = store
        .reorder_pending_message_records(
            "thread-reorder-error",
            &["pending-2".to_string(), "missing".to_string()],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));

    let pending = store
        .load_pending_message_records("thread-reorder-error")
        .await
        .unwrap();
    let ids = pending
        .iter()
        .map(|record| record.pending_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["pending-1", "pending-2"]);
    assert_eq!(pending[0].position, 1);
    assert_eq!(pending[1].position, 2);
}

#[tokio::test]
async fn pending_reorder_stale_order_reports_version_conflict() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);
    store
        .append_pending_message_records(
            "thread-reorder-conflict",
            &[
                Message::user("first").with_id("pending-1".to_string()),
                Message::user("second").with_id("pending-2".to_string()),
            ],
            mode,
        )
        .await
        .unwrap();

    let err = store
        .reorder_pending_message_records("thread-reorder-conflict", &["pending-2".to_string()])
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
async fn pending_edit_rejects_message_id_change() {
    let store = InMemoryStore::new();
    store
        .append_pending_message_records(
            "thread-edit-id",
            &[Message::user("first").with_id("pending-1".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let err = store
        .update_pending_message_record(
            "thread-edit-id",
            "pending-1",
            Message::user("renamed").with_id("other-id".to_string()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("cannot change message id"))
    );

    let pending = store
        .load_pending_message_records("thread-edit-id")
        .await
        .unwrap();
    assert_eq!(pending[0].pending_id, "pending-1");
    assert_eq!(pending[0].message.id.as_deref(), Some("pending-1"));
    assert_eq!(pending[0].message.text(), "first");
}

#[tokio::test]
async fn pending_append_assigns_message_id_and_rejects_duplicate_ids() {
    let store = InMemoryStore::new();
    let mode = DeliveryMode::new_run(DeliveryGranularity::Batch);

    let records = store
        .append_pending_message_records(
            "thread-pending-id",
            &[Message::user("first")],
            mode.clone(),
        )
        .await
        .unwrap();
    let generated_id = records[0].pending_id.clone();
    assert_eq!(
        records[0].message.id.as_deref(),
        Some(generated_id.as_str())
    );

    let err = store
        .append_pending_message_records(
            "thread-pending-id",
            &[Message::user("again").with_id(generated_id)],
            mode,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Validation(message) if message.contains("already exists")));
}

#[tokio::test]
async fn freeze_pending_moves_selected_messages_to_committed_log() {
    let store = InMemoryStore::new();
    store
        .save_messages(
            "thread-freeze",
            &[Message::user("committed").with_id("committed-1".to_string())],
        )
        .await
        .unwrap();
    store
        .append_pending_message_records(
            "thread-freeze",
            &[
                Message::user("next-step").with_id("pending-next".to_string()),
                Message::user("new-run").with_id("pending-new".to_string()),
            ],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let frozen = store
        .freeze_pending_message_records("thread-freeze", DeliveryBoundary::NextStep, Some(1))
        .await
        .unwrap();

    assert_eq!(frozen.len(), 2);
    assert_eq!(frozen[0].seq, 2);
    assert_eq!(frozen[1].seq, 3);
    let committed = store.load_messages("thread-freeze").await.unwrap().unwrap();
    assert_eq!(committed.len(), 3);
    let pending = store
        .load_pending_message_records("thread-freeze")
        .await
        .unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn freeze_pending_rejects_stale_committed_version_without_consuming() {
    let store = InMemoryStore::new();
    store
        .save_messages("thread-stale-freeze", &[Message::user("committed")])
        .await
        .unwrap();
    store
        .append_pending_message_records(
            "thread-stale-freeze",
            &[Message::user("pending").with_id("pending-stale".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let err = store
        .freeze_pending_message_records("thread-stale-freeze", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::VersionConflict { .. }));
    assert_eq!(
        store
            .load_pending_message_records("thread-stale-freeze")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn freeze_pending_with_run_commits_messages_and_run_atomically() {
    let store = InMemoryStore::new();
    store
        .append_pending_message_records(
            "thread-freeze-run",
            &[Message::user("pending").with_id("pending-run".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let run = make_run("run-freeze", "thread-freeze-run", RunStatus::Created);

    let frozen = store
        .freeze_pending_message_records_with_run(
            "thread-freeze-run",
            DeliveryBoundary::NewRun,
            Some(0),
            &["pending-run".to_string()],
            &run,
        )
        .await
        .unwrap();

    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen[0].seq, 1);
    assert_eq!(
        store
            .load_run("run-freeze")
            .await
            .unwrap()
            .unwrap()
            .thread_id,
        "thread-freeze-run"
    );
    assert!(
        store
            .load_pending_message_records("thread-freeze-run")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn freeze_pending_with_run_rejects_stale_version_without_run_write() {
    let store = InMemoryStore::new();
    store
        .save_messages("thread-freeze-run-stale", &[Message::user("committed")])
        .await
        .unwrap();
    store
        .append_pending_message_records(
            "thread-freeze-run-stale",
            &[Message::user("pending").with_id("pending-run-stale".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let run = make_run(
        "run-freeze-stale",
        "thread-freeze-run-stale",
        RunStatus::Created,
    );

    let err = store
        .freeze_pending_message_records_with_run(
            "thread-freeze-run-stale",
            DeliveryBoundary::NewRun,
            Some(0),
            &["pending-run-stale".to_string()],
            &run,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, StorageError::VersionConflict { .. }));
    assert!(store.load_run("run-freeze-stale").await.unwrap().is_none());
    assert_eq!(
        store
            .load_pending_message_records("thread-freeze-run-stale")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn freeze_pending_with_run_reports_selected_id_conflict() {
    let store = InMemoryStore::new();
    store
        .append_pending_message_records(
            "thread-freeze-selection-conflict",
            &[Message::user("a").with_id("a-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let run = make_run(
        "run-freeze-selection-conflict",
        "thread-freeze-selection-conflict",
        RunStatus::Created,
    );

    let err = store
        .freeze_pending_message_records_with_run(
            "thread-freeze-selection-conflict",
            DeliveryBoundary::NewRun,
            Some(0),
            &["b-id".to_string()],
            &run,
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        StorageError::PendingSelectionConflict {
            expected_ids,
            actual_ids
        } if expected_ids == vec!["b-id".to_string()]
            && actual_ids == vec!["a-id".to_string()]
    ));
}

#[tokio::test]
async fn checkpoint_append_same_id_rejects_projection_update() {
    let store = InMemoryStore::new();
    store
        .save_messages(
            "thread-append-only",
            &[Message::user("old").with_id("msg-1".to_string())],
        )
        .await
        .unwrap();
    let run = make_run("run-append-only", "thread-append-only", RunStatus::Created);

    let err = store
        .checkpoint_append(
            "thread-append-only",
            &[Message::user("new").with_id("msg-1".to_string())],
            Some(1),
            &run,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("already committed"))
    );
    let committed = store
        .load_committed_messages("thread-append-only")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].text(), "old");
}

#[tokio::test]
async fn pending_mutation_rejects_already_consumed_message() {
    let store = InMemoryStore::new();
    store
        .append_pending_message_records(
            "thread-consumed",
            &[Message::user("pending").with_id("pending-consumed".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    store
        .freeze_pending_message_records("thread-consumed", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap();

    let err = store
        .update_pending_message_record(
            "thread-consumed",
            "pending-consumed",
            Message::user("too late"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("already consumed"))
    );

    let err = store
        .reorder_pending_message_records("thread-consumed", &["pending-consumed".to_string()])
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("already consumed"))
    );
}

// ── RunStore ──

#[tokio::test]
async fn run_create_and_load() {
    let store = InMemoryStore::new();
    let run = make_run("r-1", "t-1", RunStatus::Running);
    store.create_run(&run).await.unwrap();
    let loaded = store.load_run("r-1").await.unwrap().unwrap();
    assert_eq!(loaded.thread_id, "t-1");
}

#[tokio::test]
async fn run_create_duplicate_returns_already_exists() {
    let store = InMemoryStore::new();
    let run = make_run("r-1", "t-1", RunStatus::Running);
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
async fn run_load_missing_returns_none() {
    let store = InMemoryStore::new();
    assert!(store.load_run("no-such").await.unwrap().is_none());
}

#[tokio::test]
async fn run_latest_returns_most_recently_updated() {
    let store = InMemoryStore::new();
    let mut run1 = make_run("r-1", "t-1", RunStatus::Running);
    run1.updated_at = 100;
    let mut run2 = make_run("r-2", "t-1", RunStatus::Done);
    run2.updated_at = 200;
    store.create_run(&run1).await.unwrap();
    store.create_run(&run2).await.unwrap();

    let latest = store.latest_run("t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r-2");
}

#[tokio::test]
async fn run_list_filters_by_thread_and_status() {
    let store = InMemoryStore::new();
    store
        .create_run(&make_run("r-1", "t-1", RunStatus::Running))
        .await
        .unwrap();
    store
        .create_run(&make_run("r-2", "t-1", RunStatus::Done))
        .await
        .unwrap();
    store
        .create_run(&make_run("r-3", "t-2", RunStatus::Running))
        .await
        .unwrap();

    let query = RunQuery {
        thread_id: Some("t-1".to_string()),
        status: Some(RunStatus::Running),
        offset: 0,
        limit: 100,
    };
    let page = store.list_runs(&query).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].run_id, "r-1");
}

// ── Concurrent mutations ──

#[tokio::test]
async fn concurrent_thread_mutations_are_safe() {
    let store = std::sync::Arc::new(InMemoryStore::new());
    let mut handles = Vec::new();
    for i in 0..10 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let mut t = Thread::new();
            t.id = format!("concurrent-{i}");
            s.save_thread(&t).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let threads = store.list_threads(0, 100).await.unwrap();
    assert_eq!(threads.len(), 10);
}

#[tokio::test]
async fn concurrent_run_mutations_are_safe() {
    let store = std::sync::Arc::new(InMemoryStore::new());
    let mut handles = Vec::new();
    for i in 0..10 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let run = make_run(&format!("r-{i}"), "t-1", RunStatus::Running);
            s.create_run(&run).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let page = store
        .list_runs(&RunQuery {
            thread_id: None,
            status: None,
            offset: 0,
            limit: 200,
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 10);
}

// ── Checkpoint atomicity ──

#[tokio::test]
async fn checkpoint_saves_messages_and_run_together() {
    let store = InMemoryStore::new();
    let msgs = vec![Message::user("checkpoint")];
    let run = make_run("r-cp", "t-1", RunStatus::Running);

    store.checkpoint("t-1", &msgs, &run).await.unwrap();

    let loaded_msgs = store.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 1);
    let loaded_run = store.load_run("r-cp").await.unwrap().unwrap();
    assert_eq!(loaded_run.thread_id, "t-1");
}

// ── Large payload ──

#[tokio::test]
async fn large_payload_handling() {
    let store = InMemoryStore::new();
    let large_text = "x".repeat(1_000_000);
    let msgs = vec![Message::user(&large_text)];
    store.save_messages("t-large", &msgs).await.unwrap();
    let loaded = store.load_messages("t-large").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 1);
}

// ── Update thread metadata ──

#[tokio::test]
async fn update_thread_metadata_on_missing_thread_returns_not_found() {
    let store = InMemoryStore::new();
    let err = store
        .update_thread_metadata("no-such", Default::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));
}

#[tokio::test]
async fn update_thread_metadata_success() {
    let store = InMemoryStore::new();
    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();

    let meta = awaken_contract::thread::ThreadMetadata {
        title: Some("Updated".to_string()),
        ..Default::default()
    };
    store
        .update_thread_metadata(&thread.id, meta)
        .await
        .unwrap();

    let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("Updated"));
}

// ── ProfileStore ──

#[tokio::test]
async fn profile_set_and_get() {
    let store = InMemoryStore::new();
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
async fn profile_get_missing() {
    let store = InMemoryStore::new();
    let result = ProfileStore::get(&store, &ProfileOwner::System, "nonexistent")
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn profile_upsert_overwrites() {
    let store = InMemoryStore::new();
    let owner = ProfileOwner::System;
    store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
    store.set(&owner, "k", serde_json::json!(2)).await.unwrap();
    let entry = ProfileStore::get(&store, &owner, "k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.value, serde_json::json!(2));
}

#[tokio::test]
async fn profile_delete_idempotent() {
    let store = InMemoryStore::new();
    let owner = ProfileOwner::Agent("bob".into());
    // Delete non-existent key is fine
    ProfileStore::delete(&store, &owner, "missing")
        .await
        .unwrap();
    // Set then delete
    store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
    ProfileStore::delete(&store, &owner, "k").await.unwrap();
    assert!(
        ProfileStore::get(&store, &owner, "k")
            .await
            .unwrap()
            .is_none()
    );
    // Delete again is fine
    ProfileStore::delete(&store, &owner, "k").await.unwrap();
}

#[tokio::test]
async fn profile_list_sorted_and_isolated() {
    let store = InMemoryStore::new();
    let alice = ProfileOwner::Agent("alice".into());
    let bob = ProfileOwner::Agent("bob".into());
    store
        .set(&alice, "b", serde_json::json!("second"))
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
    assert_eq!(entries[1].key, "b");

    // Bob's entries are isolated
    let bob_entries = ProfileStore::list(&store, &bob).await.unwrap();
    assert_eq!(bob_entries.len(), 1);
    assert_eq!(bob_entries[0].key, "x");
}

#[tokio::test]
async fn profile_clear_owner() {
    let store = InMemoryStore::new();
    let alice = ProfileOwner::Agent("alice".into());
    let bob = ProfileOwner::Agent("bob".into());
    store.set(&alice, "a", serde_json::json!(1)).await.unwrap();
    store.set(&alice, "b", serde_json::json!(2)).await.unwrap();
    store.set(&bob, "c", serde_json::json!(3)).await.unwrap();

    store.clear_owner(&alice).await.unwrap();
    assert!(ProfileStore::list(&store, &alice).await.unwrap().is_empty());
    assert_eq!(ProfileStore::list(&store, &bob).await.unwrap().len(), 1);

    // Clear again is idempotent
    store.clear_owner(&alice).await.unwrap();
}

// ── ConfigChangeNotifier ──

// ── ConfigStore::put_if_revision ──

#[tokio::test]
async fn put_if_revision_succeeds_when_revision_matches() {
    use awaken_contract::contract::config_store::ConfigStore;
    let store = InMemoryStore::new();

    // First write: no existing record, expected=0 → insert at revision 1.
    let value_r1 = serde_json::json!({"spec": {"id": "a"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
    store
        .put_if_revision("ns", "a", &value_r1, 0)
        .await
        .unwrap();
    let stored = ConfigStore::get(&store, "ns", "a").await.unwrap().unwrap();
    assert_eq!(stored["meta"]["revision"], 1);

    // Second write: expected=1 → update to revision 2.
    let value_r2 = serde_json::json!({"spec": {"id": "a"}, "meta": {"source": {"kind": "user"}, "revision": 2}});
    store
        .put_if_revision("ns", "a", &value_r2, 1)
        .await
        .unwrap();
    let stored = ConfigStore::get(&store, "ns", "a").await.unwrap().unwrap();
    assert_eq!(stored["meta"]["revision"], 2);
}

#[tokio::test]
async fn put_if_revision_returns_conflict_on_mismatch() {
    use awaken_contract::contract::storage::StorageError;
    let store = InMemoryStore::new();

    // Insert a record at revision 1.
    let value_r1 =
        serde_json::json!({"spec": {}, "meta": {"source": {"kind": "user"}, "revision": 1}});
    store.put("ns", "b", &value_r1).await.unwrap();

    // Try with wrong expected revision.
    let err = store
        .put_if_revision("ns", "b", &value_r1, 5)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 5,
            actual: 1
        }
    ));
}

#[tokio::test]
async fn put_if_absent_inserts_once_and_reports_existing() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;

    let store = InMemoryStore::new();
    let value = serde_json::json!({
        "spec": {"id": "new"},
        "meta": {"source": {"kind": "user"}, "revision": 1}
    });

    store.put_if_absent("ns", "new", &value).await.unwrap();

    let err = store.put_if_absent("ns", "new", &value).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(id) if id == "ns/new"));

    let stored = ConfigStore::get(&store, "ns", "new")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, value);
}

#[tokio::test]
async fn delete_if_revision_removes_only_matching_revision() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;

    let store = InMemoryStore::new();
    let value = serde_json::json!({
        "spec": {"id": "delete-me"},
        "meta": {"source": {"kind": "user"}, "revision": 3}
    });
    store.put("ns", "delete-me", &value).await.unwrap();

    let err = store
        .delete_if_revision("ns", "delete-me", 2)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 2,
            actual: 3
        }
    ));
    assert!(
        ConfigStore::get(&store, "ns", "delete-me")
            .await
            .unwrap()
            .is_some()
    );

    store
        .delete_if_revision("ns", "delete-me", 3)
        .await
        .unwrap();
    assert!(
        ConfigStore::get(&store, "ns", "delete-me")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn put_if_revision_handles_concurrent_writers() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;
    use std::sync::Arc;

    let store = Arc::new(InMemoryStore::new());

    // Seed with revision 0 (absence treated as 0).
    const N: usize = 20;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
                let value = serde_json::json!({"spec": {}, "meta": {"source": {"kind": "user"}, "revision": 1}});
                s.put_if_revision("ns", "concurrent", &value, 0).await
            }));
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let successes = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(StorageError::VersionConflict { expected: 0, .. })))
        .count();

    assert_eq!(successes, 1, "exactly one writer should succeed");
    assert_eq!(conflicts, N - 1, "all others should get VersionConflict");

    let stored = ConfigStore::get(store.as_ref(), "ns", "concurrent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["meta"]["revision"], 1);
}

#[tokio::test]
async fn delete_and_update_same_revision_are_mutually_exclusive() {
    use awaken_contract::contract::config_store::ConfigStore;
    use std::sync::Arc;

    let store = Arc::new(InMemoryStore::new());
    let value_r1 = serde_json::json!({
        "spec": {"id": "duel", "value": 1},
        "meta": {"source": {"kind": "user"}, "revision": 1}
    });
    store
        .put_if_revision("ns", "duel", &value_r1, 0)
        .await
        .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let delete_store = Arc::clone(&store);
    let update_store = Arc::clone(&store);
    let delete_barrier = Arc::clone(&barrier);
    let update_barrier = Arc::clone(&barrier);

    let delete = tokio::spawn(async move {
        delete_barrier.wait().await;
        delete_store.delete_if_revision("ns", "duel", 1).await
    });
    let update = tokio::spawn(async move {
        let value_r2 = serde_json::json!({
            "spec": {"id": "duel", "value": 2},
            "meta": {"source": {"kind": "user"}, "revision": 2}
        });
        update_barrier.wait().await;
        update_store
            .put_if_revision("ns", "duel", &value_r2, 1)
            .await
    });

    let delete_ok = delete.await.unwrap().is_ok();
    let update_ok = update.await.unwrap().is_ok();
    assert_ne!(
        delete_ok, update_ok,
        "same-revision delete and update must be serialized by CAS"
    );

    let stored = ConfigStore::get(store.as_ref(), "ns", "duel")
        .await
        .unwrap();
    if delete_ok {
        assert!(stored.is_none(), "successful delete must remove the record");
    } else {
        assert_eq!(
            stored.expect("successful update must leave record")["meta"]["revision"],
            2
        );
    }
}

#[tokio::test]
async fn config_change_notifier_emits_on_put_and_delete() {
    use awaken_contract::contract::config_store::{
        ConfigChangeKind, ConfigChangeNotifier, ConfigStore,
    };
    let store = InMemoryStore::new();
    let mut sub = store.subscribe().await.unwrap();

    store
        .put("agents", "a1", &serde_json::json!({"hello": "world"}))
        .await
        .unwrap();
    let event = sub.next().await.unwrap();
    assert_eq!(event.namespace, "agents");
    assert_eq!(event.id, "a1");
    assert!(matches!(event.kind, ConfigChangeKind::Put));

    ConfigStore::delete(&store, "agents", "a1").await.unwrap();
    let event = sub.next().await.unwrap();
    assert_eq!(event.namespace, "agents");
    assert_eq!(event.id, "a1");
    assert!(matches!(event.kind, ConfigChangeKind::Delete));
}

#[tokio::test]
async fn config_change_notifier_supports_multiple_subscribers() {
    use awaken_contract::contract::config_store::ConfigChangeNotifier;
    let store = InMemoryStore::new();
    let mut sub_a = store.subscribe().await.unwrap();
    let mut sub_b = store.subscribe().await.unwrap();

    store
        .put("tools", "echo", &serde_json::json!({}))
        .await
        .unwrap();

    let a = sub_a.next().await.unwrap();
    let b = sub_b.next().await.unwrap();
    assert_eq!(a.namespace, "tools");
    assert_eq!(b.namespace, "tools");
    assert_eq!(a.id, "echo");
    assert_eq!(b.id, "echo");
}
