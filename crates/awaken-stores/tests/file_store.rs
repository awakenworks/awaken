//! Integration tests for FileStore.

#![cfg(feature = "file")]

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    RunQuery, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use awaken_stores::FileStore;
use tempfile::TempDir;

mod support;
use support::make_run;

// ========================================================================
// ThreadStore
// ========================================================================

#[tokio::test]
async fn save_load_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let thread = Thread::with_id("t-1");

    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread("t-1").await.unwrap().unwrap();

    assert_eq!(loaded.id, "t-1");
}

#[tokio::test]
async fn load_nonexistent_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let loaded = store.load_thread("nonexistent").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn list_threads_paginated() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    for i in 0..5 {
        store
            .save_thread(&Thread::with_id(format!("t-{i}")))
            .await
            .unwrap();
    }
    let page1 = store.list_threads(0, 3).await.unwrap();
    assert_eq!(page1.len(), 3);
    let page2 = store.list_threads(3, 3).await.unwrap();
    assert_eq!(page2.len(), 2);
}

#[tokio::test]
async fn overwrite_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let thread = Thread::with_id("t-1").with_title("v1");
    store.save_thread(&thread).await.unwrap();

    let updated = Thread::with_id("t-1").with_title("v2");
    store.save_thread(&updated).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("v2"));
}

#[tokio::test]
async fn invalid_thread_id_rejected() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = store.load_thread("../escape").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn empty_thread_id_rejected() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = store.load_thread("").await;
    assert!(result.is_err());
}

// ========================================================================
// RunStore
// ========================================================================

#[tokio::test]
async fn create_and_load_run() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "run-1").await.unwrap().unwrap();
    assert_eq!(loaded.thread_id, "t-1");
}

#[tokio::test]
async fn create_duplicate_run_errors() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
async fn latest_run_by_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

    let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r2");
}

#[tokio::test]
async fn list_runs_with_filter() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-2", 200)).await.unwrap();

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("t-1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
}

#[tokio::test]
async fn run_with_tokens() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let mut run = make_run("r1", "t-1", 100);
    run.input_tokens = 500;
    run.output_tokens = 200;
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.input_tokens, 500);
    assert_eq!(loaded.output_tokens, 200);
}

// ========================================================================
// ThreadRunStore
// ========================================================================

#[tokio::test]
async fn checkpoint_and_load() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let run = make_run("run-x", "thread-x", 42);
    let messages = vec![Message::user("u1"), Message::assistant("a1")];

    store.checkpoint("thread-x", &messages, &run).await.unwrap();

    let loaded_messages = ThreadStore::load_messages(&store, "thread-x")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_messages.len(), 2);

    let loaded_run = RunStore::load_run(&store, "run-x").await.unwrap().unwrap();
    assert_eq!(loaded_run.thread_id, "thread-x");
}

#[tokio::test]
async fn checkpoint_overwrites_messages() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let run1 = make_run("run-1", "t-1", 100);
    store
        .checkpoint("t-1", &[Message::user("old")], &run1)
        .await
        .unwrap();

    let run2 = make_run("run-2", "t-1", 200);
    store
        .checkpoint("t-1", &[Message::user("new")], &run2)
        .await
        .unwrap();

    let msgs = ThreadStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "new");
}

#[tokio::test]
async fn load_messages_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = ThreadStore::load_messages(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn latest_run_via_thread_run_store() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let msgs = vec![Message::user("m")];
    store
        .checkpoint("t-1", &msgs, &make_run("r1", "t-1", 100))
        .await
        .unwrap();
    store
        .checkpoint("t-1", &msgs, &make_run("r2", "t-1", 200))
        .await
        .unwrap();

    let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r2");
}

#[tokio::test]
async fn thread_store_and_checkpoint_share_messages() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());

    // Save thread metadata
    store.save_thread(&Thread::with_id("t-1")).await.unwrap();

    // No messages yet
    let msgs = ThreadStore::load_messages(&store, "t-1").await.unwrap();
    assert!(msgs.is_none());

    // Checkpoint messages
    store
        .checkpoint(
            "t-1",
            &[Message::user("checkpoint")],
            &make_run("r1", "t-1", 100),
        )
        .await
        .unwrap();

    // Messages visible via both ThreadStore and ThreadRunStore
    let msgs = ThreadStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs[0].text(), "checkpoint");

    let msgs = ThreadStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs[0].text(), "checkpoint");
}

// ========================================================================
// Additional ThreadStore tests
// ========================================================================

#[tokio::test]
async fn list_threads_empty() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let ids = store.list_threads(0, 10).await.unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn list_threads_sorted() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.save_thread(&Thread::with_id("c")).await.unwrap();
    store.save_thread(&Thread::with_id("a")).await.unwrap();
    store.save_thread(&Thread::with_id("b")).await.unwrap();

    let ids = store.list_threads(0, 10).await.unwrap();
    assert_eq!(ids, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn thread_with_title() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let thread = Thread::with_id("t-1").with_title("Test Chat");
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("Test Chat"));
}

#[tokio::test]
async fn thread_serde_roundtrip_through_store() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let thread = Thread::with_id("t-1").with_title("Test");
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.id, "t-1");
    assert_eq!(loaded.metadata.title.as_deref(), Some("Test"));
}

// ========================================================================
// Additional RunStore tests
// ========================================================================

#[tokio::test]
async fn load_nonexistent_run() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = RunStore::load_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn latest_run_nonexistent_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = RunStore::latest_run(&store, "no-thread").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn list_runs_empty() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(page.total, 0);
    assert!(page.items.is_empty());
    assert!(!page.has_more);
}

#[tokio::test]
async fn list_runs_all() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    for i in 0..5 {
        store
            .create_run(&make_run(&format!("r{i}"), "t-1", i as u64 * 100))
            .await
            .unwrap();
    }
    let page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(page.items.len(), 5);
    assert!(!page.has_more);
}

#[tokio::test]
async fn list_runs_pagination() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    for i in 0..5 {
        store
            .create_run(&make_run(&format!("r{i}"), "t-1", i as u64 * 100))
            .await
            .unwrap();
    }
    let page = store
        .list_runs(&RunQuery {
            offset: 2,
            limit: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(page.items.len(), 2);
    assert!(page.has_more);
}

#[tokio::test]
async fn list_runs_filter_by_status() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let mut done = make_run("r1", "t-1", 100);
    done.status = RunStatus::Done;
    store.create_run(&done).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

    let page = store
        .list_runs(&RunQuery {
            status: Some(RunStatus::Done),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].run_id, "r1");
}

#[tokio::test]
async fn run_record_with_parent() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let mut run = make_run("r1", "t-1", 100);
    run.parent_run_id = Some("r-parent".to_string());
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.parent_run_id.as_deref(), Some("r-parent"));
}

#[tokio::test]
async fn run_record_with_termination_code() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let mut run = make_run("r1", "t-1", 100);
    run.status = RunStatus::Done;
    run.termination_code = Some("natural".to_string());
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunStatus::Done);
    assert_eq!(loaded.termination_code.as_deref(), Some("natural"));
}

// ========================================================================
// Tool call message roundtrip tests
// ========================================================================

#[tokio::test]
async fn tool_call_message_roundtrip_via_save() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());

    let tool_call = awaken_contract::contract::message::ToolCall::new(
        "call_1",
        "search",
        serde_json::json!({"query": "rust"}),
    );
    let messages = vec![
        Message::user("Find info"),
        Message::assistant_with_tool_calls("Searching...", vec![tool_call]),
        Message::tool("call_1", "Found it"),
        Message::assistant("Here are the results."),
    ];

    ThreadStore::save_messages(&store, "tool-rt", &messages)
        .await
        .unwrap();
    let loaded = ThreadStore::load_messages(&store, "tool-rt")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.len(), 4);
    let calls = loaded[1].tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "search");
    assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_1"));
}

#[tokio::test]
async fn tool_call_message_roundtrip_via_checkpoint() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());

    let tool_call = awaken_contract::contract::message::ToolCall::new(
        "call_42",
        "calculator",
        serde_json::json!({"expr": "6*7"}),
    );
    let messages = vec![
        Message::assistant_with_tool_calls("Calculating...", vec![tool_call]),
        Message::tool("call_42", r#"{"answer": 42}"#),
    ];

    store
        .checkpoint("t-1", &messages, &make_run("run-1", "t-1", 100))
        .await
        .unwrap();

    let loaded = ThreadStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.len(), 2);

    let calls = loaded[0].tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(calls[0].id, "call_42");
    assert_eq!(calls[0].name, "calculator");
    assert_eq!(loaded[1].tool_call_id.as_deref(), Some("call_42"));
}

// ========================================================================
// Cross-trait interactions
// ========================================================================

#[tokio::test]
async fn checkpoint_run_visible_via_run_store() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let run = make_run("run-cp", "t-1", 100);
    store
        .checkpoint("t-1", &[Message::user("m")], &run)
        .await
        .unwrap();

    let loaded = RunStore::load_run(&store, "run-cp").await.unwrap().unwrap();
    assert_eq!(loaded.run_id, "run-cp");

    let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "run-cp");
}

#[tokio::test]
async fn latest_run_nonexistent_thread_via_thread_run_store() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = RunStore::latest_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn load_run_nonexistent_via_thread_run_store() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = RunStore::load_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

// ========================================================================
// delete_thread / delete_messages / update_thread_metadata
// ========================================================================

#[tokio::test]
async fn delete_thread_removes_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.save_thread(&Thread::with_id("t-1")).await.unwrap();

    store.delete_thread("t-1").await.unwrap();
    let loaded = store.load_thread("t-1").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn delete_thread_not_found() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    // delete_thread is idempotent — deleting a non-existent thread succeeds silently
    store.delete_thread("missing").await.unwrap();
}

#[tokio::test]
async fn delete_messages_removes_messages() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.save_thread(&Thread::with_id("t-1")).await.unwrap();
    store
        .save_messages("t-1", &[Message::user("hello")])
        .await
        .unwrap();

    store.delete_messages("t-1").await.unwrap();
    let loaded = store.load_messages("t-1").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn delete_messages_thread_not_found() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let err = store.delete_messages("missing").await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));
}

#[tokio::test]
async fn delete_messages_no_messages_is_ok() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store.save_thread(&Thread::with_id("t-1")).await.unwrap();
    store.delete_messages("t-1").await.unwrap();
}

#[tokio::test]
async fn update_thread_metadata_changes_metadata() {
    use awaken_contract::thread::ThreadMetadata;

    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store
        .save_thread(&Thread::with_id("t-1").with_title("old"))
        .await
        .unwrap();

    let new_meta = ThreadMetadata {
        title: Some("new title".to_string()),
        updated_at: Some(12345),
        ..Default::default()
    };
    store.update_thread_metadata("t-1", new_meta).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("new title"));
    assert_eq!(loaded.metadata.updated_at, Some(12345));
}

#[tokio::test]
async fn update_thread_metadata_not_found() {
    use awaken_contract::thread::ThreadMetadata;

    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let err = store
        .update_thread_metadata("missing", ThreadMetadata::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)));
}
