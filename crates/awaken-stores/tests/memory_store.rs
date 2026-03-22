//! Integration tests for InMemoryStore.

use std::sync::Arc;

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MailboxEntry, MailboxStore, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore,
    ThreadStore,
};
use awaken_contract::thread::Thread;
use awaken_stores::InMemoryStore;

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
    let store = InMemoryStore::new();
    let thread = Thread::with_id("t-1");

    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread("t-1").await.unwrap().unwrap();

    assert_eq!(loaded.id, "t-1");
}

#[tokio::test]
async fn load_nonexistent_thread() {
    let store = InMemoryStore::new();
    let loaded = store.load_thread("nonexistent").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn list_threads_empty() {
    let store = InMemoryStore::new();
    let ids = store.list_threads(0, 10).await.unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn list_threads_paginated() {
    let store = InMemoryStore::new();
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
    let page3 = store.list_threads(5, 3).await.unwrap();
    assert!(page3.is_empty());
}

#[tokio::test]
async fn list_threads_sorted() {
    let store = InMemoryStore::new();
    store.save_thread(&Thread::with_id("c")).await.unwrap();
    store.save_thread(&Thread::with_id("a")).await.unwrap();
    store.save_thread(&Thread::with_id("b")).await.unwrap();

    let ids = store.list_threads(0, 10).await.unwrap();
    assert_eq!(ids, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn overwrite_thread() {
    let store = InMemoryStore::new();
    let thread = Thread::with_id("t-1").with_title("v1");
    store.save_thread(&thread).await.unwrap();

    let updated = Thread::with_id("t-1").with_title("v2");
    store.save_thread(&updated).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("v2"));
}

#[tokio::test]
async fn thread_with_title() {
    let store = InMemoryStore::new();
    let thread = Thread::with_id("t-1").with_title("Test Chat");
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("Test Chat"));
}

#[tokio::test]
async fn thread_serde_roundtrip_through_store() {
    let store = InMemoryStore::new();
    let thread = Thread::with_id("t-1").with_title("Test");
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.id, "t-1");
    assert_eq!(loaded.metadata.title.as_deref(), Some("Test"));
}

// ========================================================================
// RunStore
// ========================================================================

#[tokio::test]
async fn create_and_load_run() {
    let store = InMemoryStore::new();
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "run-1").await.unwrap().unwrap();
    assert_eq!(loaded.thread_id, "t-1");
    assert_eq!(loaded.updated_at, 100);
}

#[tokio::test]
async fn create_duplicate_run_errors() {
    let store = InMemoryStore::new();
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
async fn load_nonexistent_run() {
    let store = InMemoryStore::new();
    let result = RunStore::load_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn latest_run_by_thread() {
    let store = InMemoryStore::new();
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
    store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

    let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r2");
}

#[tokio::test]
async fn latest_run_nonexistent_thread() {
    let store = InMemoryStore::new();
    let result = RunStore::latest_run(&store, "no-thread").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn list_runs_all() {
    let store = InMemoryStore::new();
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
async fn list_runs_filter_by_thread() {
    let store = InMemoryStore::new();
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
    store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("t-1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 2);
}

#[tokio::test]
async fn list_runs_filter_by_status() {
    let store = InMemoryStore::new();
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
async fn list_runs_pagination() {
    let store = InMemoryStore::new();
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
async fn list_runs_empty() {
    let store = InMemoryStore::new();
    let page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(page.total, 0);
    assert!(page.items.is_empty());
    assert!(!page.has_more);
}

#[tokio::test]
async fn run_record_with_tokens() {
    let store = InMemoryStore::new();
    let mut run = make_run("r1", "t-1", 100);
    run.input_tokens = 500;
    run.output_tokens = 200;
    run.steps = 3;
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.input_tokens, 500);
    assert_eq!(loaded.output_tokens, 200);
    assert_eq!(loaded.steps, 3);
}

#[tokio::test]
async fn run_record_with_parent() {
    let store = InMemoryStore::new();
    let mut run = make_run("r1", "t-1", 100);
    run.parent_run_id = Some("r-parent".to_string());
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.parent_run_id.as_deref(), Some("r-parent"));
}

#[tokio::test]
async fn run_record_with_termination_code() {
    let store = InMemoryStore::new();
    let mut run = make_run("r1", "t-1", 100);
    run.status = RunStatus::Done;
    run.termination_code = Some("natural".to_string());
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunStatus::Done);
    assert_eq!(loaded.termination_code.as_deref(), Some("natural"));
}

// ========================================================================
// MailboxStore
// ========================================================================

#[tokio::test]
async fn mailbox_push_and_peek() {
    let store = InMemoryStore::new();
    store
        .push_message(&make_mailbox_entry("e1", "inbox-a"))
        .await
        .unwrap();
    store
        .push_message(&make_mailbox_entry("e2", "inbox-a"))
        .await
        .unwrap();

    let peeked = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(peeked.len(), 2);

    // Peek should not remove
    let peeked_again = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(peeked_again.len(), 2);
}

#[tokio::test]
async fn mailbox_pop_removes_entries() {
    let store = InMemoryStore::new();
    store
        .push_message(&make_mailbox_entry("e1", "inbox-a"))
        .await
        .unwrap();
    store
        .push_message(&make_mailbox_entry("e2", "inbox-a"))
        .await
        .unwrap();
    store
        .push_message(&make_mailbox_entry("e3", "inbox-a"))
        .await
        .unwrap();

    let popped = store.pop_messages("inbox-a", 2).await.unwrap();
    assert_eq!(popped.len(), 2);
    assert_eq!(popped[0].entry_id, "e1");
    assert_eq!(popped[1].entry_id, "e2");

    let remaining = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].entry_id, "e3");
}

#[tokio::test]
async fn mailbox_pop_empty() {
    let store = InMemoryStore::new();
    let popped = store.pop_messages("nonexistent", 10).await.unwrap();
    assert!(popped.is_empty());
}

#[tokio::test]
async fn mailbox_peek_empty() {
    let store = InMemoryStore::new();
    let peeked = store.peek_messages("nonexistent", 10).await.unwrap();
    assert!(peeked.is_empty());
}

#[tokio::test]
async fn mailbox_multiple_mailboxes() {
    let store = InMemoryStore::new();
    store
        .push_message(&make_mailbox_entry("e1", "inbox-a"))
        .await
        .unwrap();
    store
        .push_message(&make_mailbox_entry("e2", "inbox-b"))
        .await
        .unwrap();

    let a = store.peek_messages("inbox-a", 10).await.unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].entry_id, "e1");

    let b = store.peek_messages("inbox-b", 10).await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].entry_id, "e2");
}

#[tokio::test]
async fn mailbox_pop_limited() {
    let store = InMemoryStore::new();
    for i in 0..5 {
        store
            .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
            .await
            .unwrap();
    }
    let popped = store.pop_messages("inbox", 3).await.unwrap();
    assert_eq!(popped.len(), 3);
    let remaining = store.peek_messages("inbox", 10).await.unwrap();
    assert_eq!(remaining.len(), 2);
}

#[tokio::test]
async fn mailbox_peek_limited() {
    let store = InMemoryStore::new();
    for i in 0..5 {
        store
            .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
            .await
            .unwrap();
    }
    let peeked = store.peek_messages("inbox", 3).await.unwrap();
    assert_eq!(peeked.len(), 3);
    // All still present
    let all = store.peek_messages("inbox", 10).await.unwrap();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn mailbox_entry_serde_roundtrip() {
    let entry = make_mailbox_entry("e1", "inbox-a");
    let json = serde_json::to_string(&entry).unwrap();
    let parsed: MailboxEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.entry_id, "e1");
    assert_eq!(parsed.mailbox_id, "inbox-a");
}

#[tokio::test]
async fn mailbox_pop_all_then_push_again() {
    let store = InMemoryStore::new();
    store
        .push_message(&make_mailbox_entry("e1", "inbox"))
        .await
        .unwrap();
    let popped = store.pop_messages("inbox", 10).await.unwrap();
    assert_eq!(popped.len(), 1);

    // Mailbox should be empty
    let empty = store.peek_messages("inbox", 10).await.unwrap();
    assert!(empty.is_empty());

    // Push again
    store
        .push_message(&make_mailbox_entry("e2", "inbox"))
        .await
        .unwrap();
    let peeked = store.peek_messages("inbox", 10).await.unwrap();
    assert_eq!(peeked.len(), 1);
    assert_eq!(peeked[0].entry_id, "e2");
}

// ========================================================================
// ThreadRunStore
// ========================================================================

#[tokio::test]
async fn checkpoint_persists_messages_and_run() {
    let store = InMemoryStore::new();
    let run = make_run("run-x", "thread-x", 42);
    let messages = vec![Message::user("u1"), Message::assistant("a1")];

    store.checkpoint("thread-x", &messages, &run).await.unwrap();

    let loaded_messages = ThreadRunStore::load_messages(&store, "thread-x")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_messages.len(), 2);
    assert_eq!(loaded_messages[0].text(), "u1");
    assert_eq!(loaded_messages[1].text(), "a1");

    let loaded_run = ThreadRunStore::load_run(&store, "run-x")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_run.thread_id, "thread-x");
    assert_eq!(loaded_run.updated_at, 42);
}

#[tokio::test]
async fn checkpoint_overwrites_previous_messages() {
    let store = InMemoryStore::new();
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

    let msgs = ThreadRunStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "new");
}

#[tokio::test]
async fn load_messages_nonexistent() {
    let store = InMemoryStore::new();
    let result = ThreadRunStore::load_messages(&store, "missing")
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn latest_run_via_thread_run_store() {
    let store = InMemoryStore::new();
    let msgs = vec![Message::user("m")];
    store
        .checkpoint("t-1", &msgs, &make_run("r1", "t-1", 100))
        .await
        .unwrap();
    store
        .checkpoint("t-1", &msgs, &make_run("r2", "t-1", 200))
        .await
        .unwrap();
    store
        .checkpoint("t-2", &msgs, &make_run("r3", "t-2", 300))
        .await
        .unwrap();

    let latest = ThreadRunStore::latest_run(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.run_id, "r2");

    let latest2 = ThreadRunStore::latest_run(&store, "t-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest2.run_id, "r3");
}

#[tokio::test]
async fn latest_run_nonexistent_thread_via_thread_run_store() {
    let store = InMemoryStore::new();
    let result = ThreadRunStore::latest_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn load_run_via_thread_run_store() {
    let store = InMemoryStore::new();
    let run = make_run("run-1", "t-1", 100);
    store
        .checkpoint("t-1", &[Message::user("m")], &run)
        .await
        .unwrap();

    let loaded = ThreadRunStore::load_run(&store, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.run_id, "run-1");
}

#[tokio::test]
async fn load_run_nonexistent_via_thread_run_store() {
    let store = InMemoryStore::new();
    let result = ThreadRunStore::load_run(&store, "missing").await.unwrap();
    assert!(result.is_none());
}

// ========================================================================
// Concurrent access
// ========================================================================

#[tokio::test]
async fn concurrent_thread_save() {
    let store = Arc::new(InMemoryStore::new());
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let thread = Thread::with_id(format!("thread-{i}"));
                store.save_thread(&thread).await.unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let ids = store.list_threads(0, 100).await.unwrap();
    assert_eq!(ids.len(), 10);
}

#[tokio::test]
async fn concurrent_run_create() {
    let store = Arc::new(InMemoryStore::new());
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let run = make_run(&format!("run-{i}"), "t-1", i as u64 * 100);
                store.create_run(&run).await.unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(page.total, 10);
}

#[tokio::test]
async fn concurrent_checkpoint() {
    let store = Arc::new(InMemoryStore::new());
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let run = make_run(&format!("run-{i}"), "t-1", i as u64 * 100);
                store
                    .checkpoint("t-1", &[Message::user(format!("msg-{i}"))], &run)
                    .await
                    .unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    // Messages should be from the last checkpoint (non-deterministic due to concurrency)
    let msgs = ThreadRunStore::load_messages(&*store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs.len(), 1);
}

#[tokio::test]
async fn concurrent_mailbox_push() {
    let store = Arc::new(InMemoryStore::new());
    let handles: Vec<_> = (0..20)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
                    .await
                    .unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let peeked = store.peek_messages("inbox", 100).await.unwrap();
    assert_eq!(peeked.len(), 20);
}

// ========================================================================
// Cross-trait interactions
// ========================================================================

#[tokio::test]
async fn thread_store_and_thread_run_store_share_runs() {
    let store = InMemoryStore::new();

    // Create run via RunStore
    let run = make_run("run-shared", "t-1", 100);
    store.create_run(&run).await.unwrap();

    // Load via ThreadRunStore
    let loaded = ThreadRunStore::load_run(&store, "run-shared")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.run_id, "run-shared");
}

#[tokio::test]
async fn checkpoint_run_visible_via_run_store() {
    let store = InMemoryStore::new();
    let run = make_run("run-cp", "t-1", 100);
    store
        .checkpoint("t-1", &[Message::user("m")], &run)
        .await
        .unwrap();

    // The run created via checkpoint should be visible via RunStore
    let loaded = RunStore::load_run(&store, "run-cp").await.unwrap().unwrap();
    assert_eq!(loaded.run_id, "run-cp");

    let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "run-cp");
}

#[tokio::test]
async fn thread_store_and_checkpoint_share_messages() {
    let store = InMemoryStore::new();

    // Save a thread (ThreadStore)
    let thread = Thread::with_id("t-1");
    store.save_thread(&thread).await.unwrap();

    // No messages yet
    let msgs = ThreadStore::load_messages(&store, "t-1").await.unwrap();
    assert!(msgs.is_none());

    // Save messages via checkpoint
    store
        .checkpoint(
            "t-1",
            &[Message::user("checkpoint msg")],
            &make_run("r1", "t-1", 100),
        )
        .await
        .unwrap();

    // Messages visible via both ThreadStore and ThreadRunStore
    let msgs = ThreadStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs[0].text(), "checkpoint msg");

    let msgs = ThreadRunStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs[0].text(), "checkpoint msg");
}

// ========================================================================
// Tool call message roundtrip tests
// ========================================================================

#[tokio::test]
async fn tool_call_message_roundtrip_via_save() {
    let store = InMemoryStore::new();

    let tool_call = awaken_contract::contract::message::ToolCall::new(
        "call_1",
        "search",
        serde_json::json!({"query": "rust"}),
    );
    let messages = vec![
        Message::user("Find info about Rust"),
        Message::assistant_with_tool_calls("Let me search for that.", vec![tool_call]),
        Message::tool("call_1", r#"{"result": "Rust is a language"}"#),
        Message::assistant("Rust is a systems programming language."),
    ];

    ThreadStore::save_messages(&store, "tool-rt", &messages)
        .await
        .unwrap();
    let loaded = ThreadStore::load_messages(&store, "tool-rt")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.len(), 4);

    // Assistant message with tool_calls
    let calls = loaded[1].tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "search");
    assert_eq!(calls[0].arguments, serde_json::json!({"query": "rust"}));

    // Tool response message
    assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(loaded[2].text(), r#"{"result": "Rust is a language"}"#);
}

#[tokio::test]
async fn tool_call_message_roundtrip_via_checkpoint() {
    let store = InMemoryStore::new();

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

    let loaded = ThreadRunStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.len(), 2);

    let calls = loaded[0]
        .tool_calls
        .as_ref()
        .expect("tool_calls lost after checkpoint");
    assert_eq!(calls[0].id, "call_42");
    assert_eq!(calls[0].name, "calculator");
    assert_eq!(calls[0].arguments, serde_json::json!({"expr": "6*7"}));

    assert_eq!(loaded[1].tool_call_id.as_deref(), Some("call_42"));
}

#[tokio::test]
async fn multi_tool_call_roundtrip() {
    let store = InMemoryStore::new();

    let calls = vec![
        awaken_contract::contract::message::ToolCall::new(
            "call_a",
            "search",
            serde_json::json!({"q": "hello"}),
        ),
        awaken_contract::contract::message::ToolCall::new(
            "call_b",
            "fetch",
            serde_json::json!({"url": "https://example.com"}),
        ),
    ];
    let messages = vec![
        Message::assistant_with_tool_calls("multi-tool call", calls),
        Message::tool("call_a", "search result"),
        Message::tool("call_b", "fetch result"),
    ];

    ThreadStore::save_messages(&store, "multi-tool", &messages)
        .await
        .unwrap();
    let loaded = ThreadStore::load_messages(&store, "multi-tool")
        .await
        .unwrap()
        .unwrap();

    let tool_calls = loaded[0].tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_a");
    assert_eq!(tool_calls[1].id, "call_b");

    assert_eq!(loaded[1].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_b"));
}

// ========================================================================
// Additional RunStore edge cases
// ========================================================================

#[tokio::test]
async fn list_runs_ordered_by_created_at() {
    let store = InMemoryStore::new();
    // Create in reverse order
    store.create_run(&make_run("r3", "t-1", 300)).await.unwrap();
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

    let page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items[0].run_id, "r1");
    assert_eq!(page.items[1].run_id, "r2");
    assert_eq!(page.items[2].run_id, "r3");
}

#[tokio::test]
async fn list_runs_combined_filter_thread_and_status() {
    let store = InMemoryStore::new();

    let mut done = make_run("r1", "t-1", 100);
    done.status = RunStatus::Done;
    store.create_run(&done).await.unwrap();

    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

    let mut done_other = make_run("r3", "t-2", 300);
    done_other.status = RunStatus::Done;
    store.create_run(&done_other).await.unwrap();

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("t-1".to_string()),
            status: Some(RunStatus::Done),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].run_id, "r1");
}

#[tokio::test]
async fn run_record_with_state() {
    use awaken_contract::state::PersistedState;
    use std::collections::HashMap;

    let store = InMemoryStore::new();
    let mut run = make_run("r1", "t-1", 100);
    let mut extensions = HashMap::new();
    extensions.insert("key".to_string(), serde_json::json!("value"));
    run.state = Some(PersistedState {
        revision: 1,
        extensions,
    });
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "r1").await.unwrap().unwrap();
    let state = loaded.state.unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.extensions["key"], serde_json::json!("value"));
}

// ========================================================================
// Additional mailbox edge cases
// ========================================================================

#[tokio::test]
async fn mailbox_fifo_ordering() {
    let store = InMemoryStore::new();
    for i in 0..5 {
        store
            .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
            .await
            .unwrap();
    }
    let popped = store.pop_messages("inbox", 5).await.unwrap();
    for (i, entry) in popped.iter().enumerate() {
        assert_eq!(entry.entry_id, format!("e{i}"), "FIFO order broken");
    }
}

// ========================================================================
// Full agent lifecycle simulation
// ========================================================================

#[tokio::test]
async fn full_agent_run_via_checkpoint() {
    let store = InMemoryStore::new();

    // 1. Save initial thread
    let thread = Thread::with_id("t-1");
    store.save_thread(&thread).await.unwrap();

    // 2. User message checkpoint
    store
        .checkpoint(
            "t-1",
            &[Message::user("What is 2+2?")],
            &make_run("run-1", "t-1", 100),
        )
        .await
        .unwrap();

    // 3. Tool call checkpoint
    let tool_call = awaken_contract::contract::message::ToolCall::new(
        "call-1",
        "calculator",
        serde_json::json!({"expr": "2+2"}),
    );
    store
        .checkpoint(
            "t-1",
            &[
                Message::user("What is 2+2?"),
                Message::assistant_with_tool_calls("Let me calculate.", vec![tool_call]),
                Message::tool("call-1", "4"),
            ],
            &make_run("run-1", "t-1", 200),
        )
        .await
        .unwrap();

    // 4. Final assistant message
    let mut final_run = make_run("run-1", "t-1", 300);
    final_run.status = RunStatus::Done;
    final_run.termination_code = Some("natural".to_string());
    final_run.steps = 2;
    final_run.input_tokens = 100;
    final_run.output_tokens = 50;

    store
        .checkpoint(
            "t-1",
            &[
                Message::user("What is 2+2?"),
                Message::assistant_with_tool_calls(
                    "Let me calculate.",
                    vec![awaken_contract::contract::message::ToolCall::new(
                        "call-1",
                        "calculator",
                        serde_json::json!({"expr": "2+2"}),
                    )],
                ),
                Message::tool("call-1", "4"),
                Message::assistant("2 + 2 = 4"),
            ],
            &final_run,
        )
        .await
        .unwrap();

    // Verify final state
    let msgs = ThreadRunStore::load_messages(&store, "t-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].text(), "What is 2+2?");
    assert_eq!(msgs[3].text(), "2 + 2 = 4");

    let loaded_run = ThreadRunStore::load_run(&store, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_run.status, RunStatus::Done);
    assert_eq!(loaded_run.steps, 2);
    assert_eq!(loaded_run.input_tokens, 100);
    assert_eq!(loaded_run.output_tokens, 50);
}

// ========================================================================
// Concurrent mixed operations
// ========================================================================

#[tokio::test]
async fn concurrent_mixed_operations() {
    let store = Arc::new(InMemoryStore::new());

    let mut handles = Vec::new();

    // Threads
    for i in 0..5 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            store
                .save_thread(&Thread::with_id(format!("thread-{i}")))
                .await
                .unwrap();
        }));
    }

    // Runs
    for i in 0..5 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            store
                .create_run(&make_run(&format!("run-{i}"), "t-1", i as u64 * 100))
                .await
                .unwrap();
        }));
    }

    // Mailbox
    for i in 0..5 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            store
                .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let thread_count = store.list_threads(0, 100).await.unwrap().len();
    assert_eq!(thread_count, 5);

    let run_page = store.list_runs(&RunQuery::default()).await.unwrap();
    assert_eq!(run_page.total, 5);

    let mailbox_count = store.peek_messages("inbox", 100).await.unwrap().len();
    assert_eq!(mailbox_count, 5);
}
