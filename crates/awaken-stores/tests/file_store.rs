//! Integration tests for FileStore.

#![cfg(feature = "file")]

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MailboxEntry, MailboxStore, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore,
    ThreadStore,
};
use awaken_contract::thread::Thread;
use awaken_stores::FileStore;
use tempfile::TempDir;

fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: run_id.to_owned(),
        thread_id: thread_id.to_owned(),
        agent_id: "agent-1".to_owned(),
        parent_run_id: None,
        status: RunStatus::Running,
        termination_code: None,
        created_at: updated_at,
        updated_at,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

fn make_mailbox_entry(id: &str, mailbox: &str) -> MailboxEntry {
    MailboxEntry {
        entry_id: id.to_string(),
        mailbox_id: mailbox.to_string(),
        payload: serde_json::json!({"text": id}),
        created_at: 1000,
    }
}

// ========================================================================
// ThreadStore
// ========================================================================

#[tokio::test]
async fn save_load_thread() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let thread = Thread::with_id("t-1").with_message(Message::user("Hello"));

    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread("t-1").await.unwrap().unwrap();

    assert_eq!(loaded.id, "t-1");
    assert_eq!(loaded.message_count(), 1);
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
    let thread = Thread::with_id("t-1").with_message(Message::user("hello"));
    store.save_thread(&thread).await.unwrap();

    let updated = thread.with_message(Message::assistant("hi"));
    store.save_thread(&updated).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.message_count(), 2);
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
// MailboxStore
// ========================================================================

#[tokio::test]
async fn mailbox_push_and_peek() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store
        .push_message(&make_mailbox_entry("e1", "inbox-a"))
        .await
        .unwrap();

    let peeked = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(peeked.len(), 1);
}

#[tokio::test]
async fn mailbox_pop_removes_files() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    store
        .push_message(&make_mailbox_entry("e1", "inbox-a"))
        .await
        .unwrap();
    store
        .push_message(&make_mailbox_entry("e2", "inbox-a"))
        .await
        .unwrap();

    let popped = store.pop_messages("inbox-a", 1).await.unwrap();
    assert_eq!(popped.len(), 1);

    let remaining = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
}

#[tokio::test]
async fn mailbox_pop_empty() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let popped = store.pop_messages("nonexistent", 10).await.unwrap();
    assert!(popped.is_empty());
}

#[tokio::test]
async fn mailbox_invalid_id_rejected() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = store.peek_messages("../escape", 10).await;
    assert!(result.is_err());
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

    let loaded_messages = store.load_messages("thread-x").await.unwrap().unwrap();
    assert_eq!(loaded_messages.len(), 2);

    let loaded_run = ThreadRunStore::load_run(&store, "run-x")
        .await
        .unwrap()
        .unwrap();
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

    let msgs = store.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "new");
}

#[tokio::test]
async fn load_messages_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());
    let result = store.load_messages("missing").await.unwrap();
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

    let latest = ThreadRunStore::latest_run(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.run_id, "r2");
}

#[tokio::test]
async fn thread_store_and_thread_run_store_are_independent() {
    let tmp = TempDir::new().unwrap();
    let store = FileStore::new(tmp.path());

    // Save thread
    store
        .save_thread(&Thread::with_id("t-1").with_message(Message::user("hello")))
        .await
        .unwrap();

    // No messages via ThreadRunStore
    let msgs = store.load_messages("t-1").await.unwrap();
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

    // Thread still has original
    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.messages[0].text(), "hello");
}
