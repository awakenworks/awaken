use serde_json::json;
use std::sync::Arc;
use tirea_contract::runtime::state::SerializedStateAction;
use tirea_contract::storage::{
    MailboxEntryOrigin, MailboxEntryStatus, MailboxQuery, MailboxReader, MailboxStoreError,
    MailboxWriter, Task, TaskQuery, TaskReader, TaskStatus, TaskStoreError, TaskWriter,
    ThreadReader, ThreadStore, ThreadStoreError, ThreadSync, ThreadWriter, VersionPrecondition,
};
use tirea_contract::testing::MailboxEntryBuilder;
use tirea_contract::thread::ThreadChangeSet;
use tirea_contract::{
    CheckpointReason, Message, MessageMetadata, MessageQuery, Role, StateScope, Thread,
    ThreadListQuery, ToolCall,
};
use tirea_state::{path, Op, Patch, TrackedPatch};
use tirea_store_adapters::MemoryStore;

#[tokio::test]
async fn test_memory_storage_save_load() {
    let storage = MemoryStore::new();
    let thread = Thread::new("test-1").with_message(Message::user("Hello"));

    storage.save(&thread).await.unwrap();
    let loaded = storage.load_thread("test-1").await.unwrap();

    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.id, "test-1");
    assert_eq!(loaded.message_count(), 1);
}

#[tokio::test]
async fn test_memory_storage_load_not_found() {
    let storage = MemoryStore::new();
    let loaded = storage.load_thread("nonexistent").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn test_memory_storage_delete() {
    let storage = MemoryStore::new();
    let thread = Thread::new("test-1");

    storage.save(&thread).await.unwrap();
    assert!(storage.load_thread("test-1").await.unwrap().is_some());

    storage.delete("test-1").await.unwrap();
    assert!(storage.load_thread("test-1").await.unwrap().is_none());
}

#[tokio::test]
async fn test_memory_storage_list() {
    let storage = MemoryStore::new();

    storage.save(&Thread::new("thread-1")).await.unwrap();
    storage.save(&Thread::new("thread-2")).await.unwrap();

    let mut ids = storage.list().await.unwrap();
    ids.sort();

    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"thread-1".to_string()));
    assert!(ids.contains(&"thread-2".to_string()));
}

#[tokio::test]
async fn test_memory_storage_update_session() {
    let storage = MemoryStore::new();

    // Save initial session
    let thread = Thread::new("test-1").with_message(Message::user("Hello"));
    storage.save(&thread).await.unwrap();

    // Update session
    let thread = thread.with_message(Message::assistant("Hi!"));
    storage.save(&thread).await.unwrap();

    // Load and verify
    let loaded = storage.load_thread("test-1").await.unwrap().unwrap();
    assert_eq!(loaded.message_count(), 2);
}

#[tokio::test]
async fn test_memory_storage_with_state_and_patches() {
    let storage = MemoryStore::new();

    let thread = Thread::with_initial_state("test-1", json!({"counter": 0}))
        .with_message(Message::user("Increment"))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(5))),
        ));

    storage.save(&thread).await.unwrap();

    let loaded = storage.load_thread("test-1").await.unwrap().unwrap();
    assert_eq!(loaded.patch_count(), 1);
    assert_eq!(loaded.state["counter"], 0);

    // Rebuild state should apply patches
    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["counter"], 5);
}

#[tokio::test]
async fn test_memory_storage_delete_nonexistent() {
    let storage = MemoryStore::new();
    // Deleting non-existent session should not error
    storage.delete("nonexistent").await.unwrap();
}

#[tokio::test]
async fn test_memory_storage_list_empty() {
    let storage = MemoryStore::new();
    let ids = storage.list().await.unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn test_memory_storage_concurrent_access() {
    use std::sync::Arc;

    let storage = Arc::new(MemoryStore::new());

    // Spawn multiple tasks
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move {
                let thread = Thread::new(format!("thread-{}", i));
                storage.save(&thread).await.unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let ids = storage.list().await.unwrap();
    assert_eq!(ids.len(), 10);
}

// ========================================================================
// Pagination tests
// ========================================================================

fn make_messages(n: usize) -> Vec<std::sync::Arc<Message>> {
    (0..n)
        .map(|i| std::sync::Arc::new(Message::user(format!("msg-{}", i))))
        .collect()
}

fn make_thread_with_messages(thread_id: &str, n: usize) -> Thread {
    let mut thread = Thread::new(thread_id);
    for msg in make_messages(n) {
        // Deref Arc to get Message for with_message
        thread = thread.with_message((*msg).clone());
    }
    thread
}

#[tokio::test]
async fn test_memory_storage_load_messages() {
    let storage = MemoryStore::new();
    let thread = make_thread_with_messages("test-1", 10);
    storage.save(&thread).await.unwrap();

    let query = MessageQuery {
        limit: 3,
        ..Default::default()
    };
    let page = ThreadReader::load_messages(&storage, "test-1", &query)
        .await
        .unwrap();

    assert_eq!(page.messages.len(), 3);
    assert!(page.has_more);
    assert_eq!(page.messages[0].message.content, "msg-0");
}

#[tokio::test]
async fn test_memory_storage_load_messages_not_found() {
    let storage = MemoryStore::new();
    let query = MessageQuery::default();
    let result = ThreadReader::load_messages(&storage, "nonexistent", &query).await;
    assert!(matches!(result, Err(ThreadStoreError::NotFound(_))));
}

#[tokio::test]
async fn test_memory_storage_message_count() {
    let storage = MemoryStore::new();
    let thread = make_thread_with_messages("test-1", 7);
    storage.save(&thread).await.unwrap();

    let count = storage.message_count("test-1").await.unwrap();
    assert_eq!(count, 7);
}

#[tokio::test]
async fn test_memory_storage_message_count_not_found() {
    let storage = MemoryStore::new();
    let result = storage.message_count("nonexistent").await;
    assert!(matches!(result, Err(ThreadStoreError::NotFound(_))));
}

// ========================================================================
// Visibility tests
// ========================================================================

fn make_mixed_visibility_thread(thread_id: &str) -> Thread {
    Thread::new(thread_id)
        .with_message(Message::user("user-0"))
        .with_message(Message::assistant("assistant-1"))
        .with_message(Message::internal_system("reminder-2"))
        .with_message(Message::user("user-3"))
        .with_message(Message::internal_system("reminder-4"))
        .with_message(Message::assistant("assistant-5"))
}

#[tokio::test]
async fn test_memory_storage_load_messages_filters_visibility() {
    let storage = MemoryStore::new();
    let thread = make_mixed_visibility_thread("test-vis");
    storage.save(&thread).await.unwrap();

    // Default query (visibility = All)
    let page = ThreadReader::load_messages(&storage, "test-vis", &MessageQuery::default())
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 4);

    // visibility = None (all messages)
    let query = MessageQuery {
        visibility: None,
        ..Default::default()
    };
    let page = ThreadReader::load_messages(&storage, "test-vis", &query)
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 6);
}

// ========================================================================
// Run ID filtering tests
// ========================================================================

fn make_multi_run_thread(thread_id: &str) -> Thread {
    Thread::new(thread_id)
        // User message (no run metadata)
        .with_message(Message::user("hello"))
        // Run A, step 0: assistant + tool
        .with_message(
            Message::assistant("thinking...").with_metadata(MessageMetadata {
                run_id: Some("run-a".to_string()),
                step_index: Some(0),
            }),
        )
        .with_message(
            Message::tool("tc1", "result").with_metadata(MessageMetadata {
                run_id: Some("run-a".to_string()),
                step_index: Some(0),
            }),
        )
        // Run A, step 1: assistant final
        .with_message(Message::assistant("done").with_metadata(MessageMetadata {
            run_id: Some("run-a".to_string()),
            step_index: Some(1),
        }))
        // User follow-up (no run metadata)
        .with_message(Message::user("more"))
        // Run B, step 0
        .with_message(Message::assistant("ok").with_metadata(MessageMetadata {
            run_id: Some("run-b".to_string()),
            step_index: Some(0),
        }))
}

#[tokio::test]
async fn test_memory_storage_load_messages_by_run_id() {
    let storage = MemoryStore::new();
    let thread = make_multi_run_thread("test-run");
    storage.save(&thread).await.unwrap();

    let query = MessageQuery {
        run_id: Some("run-a".to_string()),
        visibility: None,
        ..Default::default()
    };
    let page = ThreadReader::load_messages(&storage, "test-run", &query)
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 3);
}

// ========================================================================
// Thread list pagination tests
// ========================================================================

#[tokio::test]
async fn test_list_paginated_default() {
    let storage = MemoryStore::new();
    for i in 0..5 {
        storage
            .save(&Thread::new(format!("s-{i:02}")))
            .await
            .unwrap();
    }
    let page = storage
        .list_paginated(&ThreadListQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 5);
    assert_eq!(page.total, 5);
    assert!(!page.has_more);
}

#[tokio::test]
async fn test_list_paginated_with_limit() {
    let storage = MemoryStore::new();
    for i in 0..10 {
        storage
            .save(&Thread::new(format!("s-{i:02}")))
            .await
            .unwrap();
    }
    let page = storage
        .list_paginated(&ThreadListQuery {
            offset: 0,
            limit: 3,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.total, 10);
    assert!(page.has_more);
    // Items should be sorted.
    assert_eq!(page.items, vec!["s-00", "s-01", "s-02"]);
}

#[tokio::test]
async fn test_list_paginated_with_offset() {
    let storage = MemoryStore::new();
    for i in 0..5 {
        storage
            .save(&Thread::new(format!("s-{i:02}")))
            .await
            .unwrap();
    }
    let page = storage
        .list_paginated(&ThreadListQuery {
            offset: 3,
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 5);
    assert!(!page.has_more);
    assert_eq!(page.items, vec!["s-03", "s-04"]);
}

#[tokio::test]
async fn test_list_paginated_offset_beyond_total() {
    let storage = MemoryStore::new();
    for i in 0..3 {
        storage
            .save(&Thread::new(format!("s-{i:02}")))
            .await
            .unwrap();
    }
    let page = storage
        .list_paginated(&ThreadListQuery {
            offset: 100,
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.total, 3);
    assert!(!page.has_more);
}

#[tokio::test]
async fn test_list_paginated_empty() {
    let storage = MemoryStore::new();
    let page = storage
        .list_paginated(&ThreadListQuery::default())
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.total, 0);
    assert!(!page.has_more);
}

// ========================================================================
// ThreadWriter / ThreadReader / ThreadSync tests
// ========================================================================

fn sample_delta(run_id: &str, reason: CheckpointReason) -> ThreadChangeSet {
    ThreadChangeSet {
        run_id: run_id.to_string(),
        parent_run_id: None,
        run_meta: None,
        reason,
        messages: vec![Arc::new(Message::assistant("hello"))],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    }
}

#[tokio::test]
async fn test_thread_store_trait_object_roundtrip() {
    use std::sync::Arc;

    let agent_state_store: Arc<dyn ThreadStore> = Arc::new(MemoryStore::new());
    let thread = Thread::new("trait-object-t1").with_message(Message::user("hi"));

    agent_state_store.create(&thread).await.unwrap();
    let loaded = agent_state_store
        .load_thread("trait-object-t1")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.id, "trait-object-t1");
    assert_eq!(loaded.message_count(), 1);
}

#[tokio::test]
async fn test_thread_store_create_and_load() {
    let store = MemoryStore::new();
    let thread = Thread::new("t1").with_message(Message::user("hi"));
    let committed = store.create(&thread).await.unwrap();
    assert_eq!(committed.version, 0);

    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.version, 0);
    assert_eq!(head.thread.id, "t1");
    assert_eq!(head.thread.message_count(), 1);
}

#[tokio::test]
async fn test_thread_store_create_already_exists() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();
    let err = store.create(&Thread::new("t1")).await.unwrap_err();
    assert!(matches!(err, ThreadStoreError::AlreadyExists));
}

#[tokio::test]
async fn test_thread_store_append() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();

    let delta = sample_delta("run-1", CheckpointReason::AssistantTurnCommitted);
    let committed = store
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 1);

    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.version, 1);
    assert_eq!(head.thread.message_count(), 1); // from delta
}

#[tokio::test]
async fn test_thread_store_append_not_found() {
    let store = MemoryStore::new();
    let delta = sample_delta("run-1", CheckpointReason::RunFinished);
    let err = store
        .append("missing", &delta, VersionPrecondition::Any)
        .await
        .unwrap_err();
    assert!(matches!(err, ThreadStoreError::NotFound(_)));
}

#[tokio::test]
async fn test_thread_store_delete() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();
    ThreadWriter::delete(&store, "t1").await.unwrap();
    assert!(ThreadReader::load(&store, "t1").await.unwrap().is_none());
}

#[tokio::test]
async fn test_thread_store_append_with_snapshot() {
    let store = MemoryStore::new();
    let thread = Thread::with_initial_state("t1", json!({"counter": 0}));
    store.create(&thread).await.unwrap();

    let delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![],
        patches: vec![],
        state_actions: vec![],
        snapshot: Some(json!({"counter": 42})),
    };
    store
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.thread.state, json!({"counter": 42}));
    assert!(head.thread.patches.is_empty());
}

#[tokio::test]
async fn test_thread_sync_load_deltas() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();

    let d1 = sample_delta("run-1", CheckpointReason::UserMessage);
    let d2 = sample_delta("run-1", CheckpointReason::AssistantTurnCommitted);
    let d3 = sample_delta("run-1", CheckpointReason::RunFinished);
    store
        .append("t1", &d1, VersionPrecondition::Any)
        .await
        .unwrap();
    store
        .append("t1", &d2, VersionPrecondition::Any)
        .await
        .unwrap();
    store
        .append("t1", &d3, VersionPrecondition::Any)
        .await
        .unwrap();

    // All deltas
    let deltas = store.load_deltas("t1", 0).await.unwrap();
    assert_eq!(deltas.len(), 3);

    // Deltas after version 1
    let deltas = store.load_deltas("t1", 1).await.unwrap();
    assert_eq!(deltas.len(), 2);

    // Deltas after version 3 (none)
    let deltas = store.load_deltas("t1", 3).await.unwrap();
    assert_eq!(deltas.len(), 0);
}

#[tokio::test]
async fn test_thread_query_list_threads() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();
    store.create(&Thread::new("t2")).await.unwrap();

    let page = store
        .list_threads(&ThreadListQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 2);
}

#[tokio::test]
async fn test_thread_query_list_threads_by_parent() {
    let store = MemoryStore::new();
    store.create(&Thread::new("parent")).await.unwrap();
    store
        .create(&Thread::new("child-1").with_parent_thread_id("parent"))
        .await
        .unwrap();
    store
        .create(&Thread::new("child-2").with_parent_thread_id("parent"))
        .await
        .unwrap();
    store.create(&Thread::new("unrelated")).await.unwrap();

    let query = ThreadListQuery {
        parent_thread_id: Some("parent".to_string()),
        ..Default::default()
    };
    let page = store.list_threads(&query).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.items.contains(&"child-1".to_string()));
    assert!(page.items.contains(&"child-2".to_string()));
}

#[tokio::test]
async fn test_thread_query_load_messages() {
    let store = MemoryStore::new();
    let thread = Thread::new("t1")
        .with_message(Message::user("hello"))
        .with_message(Message::assistant("hi"));
    store.create(&thread).await.unwrap();

    let page = ThreadReader::load_messages(
        &store,
        "t1",
        &MessageQuery {
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert!(page.has_more);
}

#[tokio::test]
async fn test_parent_thread_id_persisted() {
    let thread = Thread::new("child-1").with_parent_thread_id("parent-1");
    let json_str = serde_json::to_string(&thread).unwrap();
    assert!(json_str.contains("parent_thread_id"));

    let restored: Thread = serde_json::from_str(&json_str).unwrap();
    assert_eq!(restored.parent_thread_id.as_deref(), Some("parent-1"));
}

#[tokio::test]
async fn test_parent_thread_id_none_omitted() {
    let thread = Thread::new("t1");
    let json_str = serde_json::to_string(&thread).unwrap();
    assert!(!json_str.contains("parent_thread_id"));
}

// ========================================================================
// End-to-end: ThreadChangeSet append flow (full agent lifecycle)
// ========================================================================

// ========================================================================
// Tool call message round-trip tests
// ========================================================================

/// Verify that assistant messages with tool_calls and tool response messages
/// with tool_call_id survive a save/load round-trip through MemoryStore.
#[tokio::test]
async fn test_tool_call_message_roundtrip_via_save() {
    let store = MemoryStore::new();

    let tool_call = ToolCall::new("call_1", "search", json!({"query": "rust"}));
    let thread = Thread::new("tool-rt")
        .with_message(Message::user("Find info about Rust"))
        .with_message(Message::assistant_with_tool_calls(
            "Let me search for that.",
            vec![tool_call],
        ))
        .with_message(Message::tool(
            "call_1",
            r#"{"result": "Rust is a language"}"#,
        ))
        .with_message(Message::assistant(
            "Rust is a systems programming language.",
        ));

    store.save(&thread).await.unwrap();
    let loaded = store.load_thread("tool-rt").await.unwrap().unwrap();

    assert_eq!(loaded.message_count(), 4);

    // Assistant message with tool_calls
    let assistant_msg = &loaded.messages[1];
    assert_eq!(assistant_msg.role, Role::Assistant);
    let calls = assistant_msg.tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "search");
    assert_eq!(calls[0].arguments, json!({"query": "rust"}));

    // Tool response message with tool_call_id
    let tool_msg = &loaded.messages[2];
    assert_eq!(tool_msg.role, Role::Tool);
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(tool_msg.content, r#"{"result": "Rust is a language"}"#);
}

/// Verify tool_calls survive an append → load round-trip.
#[tokio::test]
async fn test_tool_call_message_roundtrip_via_append() {
    let store = MemoryStore::new();
    store.create(&Thread::new("tool-append")).await.unwrap();

    let tool_call = ToolCall::new("call_42", "calculator", json!({"expr": "6*7"}));
    let delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![
            Arc::new(Message::assistant_with_tool_calls(
                "Calculating...",
                vec![tool_call],
            )),
            Arc::new(Message::tool("call_42", r#"{"answer": 42}"#)),
        ],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    store
        .append("tool-append", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    let head = ThreadReader::load(&store, "tool-append")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.thread.message_count(), 2);

    let calls = head.thread.messages[0]
        .tool_calls
        .as_ref()
        .expect("tool_calls lost after append");
    assert_eq!(calls[0].id, "call_42");
    assert_eq!(calls[0].name, "calculator");
    assert_eq!(calls[0].arguments, json!({"expr": "6*7"}));

    assert_eq!(
        head.thread.messages[1].tool_call_id.as_deref(),
        Some("call_42")
    );
}

/// Verify tool_calls survive append → load_deltas round-trip via ThreadSync.
#[tokio::test]
async fn test_tool_call_message_roundtrip_via_load_deltas() {
    let store = MemoryStore::new();
    store.create(&Thread::new("tool-sync")).await.unwrap();

    let calls = vec![
        ToolCall::new("call_a", "search", json!({"q": "hello"})),
        ToolCall::new("call_b", "fetch", json!({"url": "https://example.com"})),
    ];
    let delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![
            Arc::new(Message::assistant_with_tool_calls("multi-tool", calls)),
            Arc::new(Message::tool("call_a", "search result")),
            Arc::new(Message::tool("call_b", "fetch result")),
        ],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    store
        .append("tool-sync", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    let deltas = store.load_deltas("tool-sync", 0).await.unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].messages.len(), 3);

    let assistant = &deltas[0].messages[0];
    let tool_calls = assistant
        .tool_calls
        .as_ref()
        .expect("tool_calls lost in delta");
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_a");
    assert_eq!(tool_calls[1].id, "call_b");

    assert_eq!(
        deltas[0].messages[1].tool_call_id.as_deref(),
        Some("call_a")
    );
    assert_eq!(
        deltas[0].messages[2].tool_call_id.as_deref(),
        Some("call_b")
    );
}

/// Verify tool call messages survive load_messages pagination.
#[tokio::test]
async fn test_tool_call_message_roundtrip_via_load_messages() {
    let store = MemoryStore::new();
    let tool_call = ToolCall::new("call_pg", "search", json!({"q": "test"}));
    let thread = Thread::new("tool-paged")
        .with_message(Message::user("search"))
        .with_message(Message::assistant_with_tool_calls(
            "searching",
            vec![tool_call],
        ))
        .with_message(Message::tool("call_pg", "found it"));

    store.save(&thread).await.unwrap();

    let page = ThreadReader::load_messages(
        &store,
        "tool-paged",
        &MessageQuery {
            visibility: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(page.messages.len(), 3);

    let assistant = &page.messages[1].message;
    let calls = assistant
        .tool_calls
        .as_ref()
        .expect("tool_calls lost in pagination");
    assert_eq!(calls[0].id, "call_pg");

    let tool = &page.messages[2].message;
    assert_eq!(tool.tool_call_id.as_deref(), Some("call_pg"));
}

// ========================================================================
// End-to-end: ThreadChangeSet append flow (full agent lifecycle)
// ========================================================================

/// Simulates a complete agent run: create → user message → assistant turn →
/// tool results → run finished, all via append().
#[tokio::test]
async fn test_full_agent_run_via_append() {
    let store = MemoryStore::new();

    // 1. Create thread
    let thread = Thread::new("t1");
    let committed = store.create(&thread).await.unwrap();
    assert_eq!(committed.version, 0);

    // 2. User message delta (simulates http handler)
    let mut thread = thread.with_message(Message::user("What is 2+2?"));

    let user_delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::UserMessage,
        messages: vec![Arc::new(Message::user("What is 2+2?"))],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    let committed = store
        .append("t1", &user_delta, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 1);

    // 3. Assistant turn committed (LLM inference)
    thread = thread.with_message(Message::assistant("2+2 = 4"));

    let assistant_delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("2+2 = 4"))],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    let committed = store
        .append("t1", &assistant_delta, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 2);

    // 4. Tool results committed (with patches)
    let patch = TrackedPatch::new(Patch::new().with_op(Op::set(path!("result"), json!(4))));
    thread = thread
        .with_message(Message::tool("call-1", "4"))
        .with_patch(patch.clone());

    let tool_delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![Arc::new(Message::tool("call-1", "4"))],
        patches: vec![patch],
        state_actions: vec![],
        snapshot: None,
    };
    let committed = store
        .append("t1", &tool_delta, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 3);

    // 5. Run finished (final assistant message)
    let _thread = thread.with_message(Message::assistant("The answer is 4."));

    let finished_delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![Arc::new(Message::assistant("The answer is 4."))],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    let committed = store
        .append("t1", &finished_delta, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 4);

    // 6. Verify final state
    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.version, 4);
    assert_eq!(head.thread.message_count(), 4); // user + assistant + tool + assistant
    assert_eq!(head.thread.patch_count(), 1);

    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["result"], 4);
}

/// Verify ThreadSync::load_deltas can replay the full run history.
#[tokio::test]
async fn test_delta_replay_reconstructs_thread() {
    let store = MemoryStore::new();
    let thread = Thread::with_initial_state("t1", json!({"count": 0}));
    store.create(&thread).await.unwrap();

    // Simulate 3 rounds of append
    let deltas: Vec<ThreadChangeSet> = vec![
        ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::UserMessage,
            messages: vec![Arc::new(Message::user("inc"))],
            patches: vec![TrackedPatch::new(
                Patch::new().with_op(Op::increment(path!("count"), 1)),
            )],
            state_actions: vec![],
            snapshot: None,
        },
        ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant("done"))],
            patches: vec![TrackedPatch::new(
                Patch::new().with_op(Op::increment(path!("count"), 1)),
            )],
            state_actions: vec![],
            snapshot: None,
        },
        ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::RunFinished,
            messages: vec![],
            patches: vec![],
            state_actions: vec![],
            snapshot: None,
        },
    ];

    for delta in &deltas {
        store
            .append("t1", delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }

    // Replay from scratch
    let all_deltas = store.load_deltas("t1", 0).await.unwrap();
    assert_eq!(all_deltas.len(), 3);

    // Reconstruct thread from empty + deltas
    let mut reconstructed = Thread::with_initial_state("t1", json!({"count": 0}));
    for d in &all_deltas {
        for m in &d.messages {
            reconstructed.messages.push(m.clone());
        }
        reconstructed.patches.extend(d.patches.iter().cloned());
    }

    let loaded = store.load_thread("t1").await.unwrap().unwrap();
    assert_eq!(reconstructed.message_count(), loaded.message_count());
    assert_eq!(reconstructed.patch_count(), loaded.patch_count());

    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["count"], 2);
}

/// Verify partial replay: load_deltas(after_version=1) skips early deltas.
#[tokio::test]
async fn test_partial_delta_replay() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();

    for i in 0..5u64 {
        let delta = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant(format!("msg-{i}")))],
            patches: vec![],
            state_actions: vec![],
            snapshot: None,
        };
        store
            .append("t1", &delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }

    // Only deltas after version 3 (should be versions 4 and 5)
    let deltas = store.load_deltas("t1", 3).await.unwrap();
    assert_eq!(deltas.len(), 2);
    assert_eq!(deltas[0].messages[0].content, "msg-3");
    assert_eq!(deltas[1].messages[0].content, "msg-4");
}

/// ThreadChangeSet append preserves patch content and source.
#[tokio::test]
async fn test_append_preserves_patch_provenance() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();

    let patch = TrackedPatch::new(Patch::new().with_op(Op::set(path!("key"), json!("value"))))
        .with_source("tool:weather")
        .with_description("Set weather data");

    let delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![],
        patches: vec![patch],
        state_actions: vec![],
        snapshot: None,
    };
    store
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Verify provenance survived
    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.thread.patches.len(), 1);
    assert_eq!(
        head.thread.patches[0].source.as_deref(),
        Some("tool:weather")
    );
    assert_eq!(
        head.thread.patches[0].description.as_deref(),
        Some("Set weather data")
    );

    // Also via ThreadSync
    let deltas = store.load_deltas("t1", 0).await.unwrap();
    assert_eq!(deltas[0].patches[0].source.as_deref(), Some("tool:weather"));
}

/// Verify parent_run_id is preserved through delta storage.
#[tokio::test]
async fn test_append_preserves_parent_run_id() {
    let store = MemoryStore::new();
    store
        .create(&Thread::new("child").with_parent_thread_id("parent"))
        .await
        .unwrap();

    let delta = ThreadChangeSet {
        run_id: "child-run-1".to_string(),
        parent_run_id: Some("parent-run-1".to_string()),
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("sub-agent reply"))],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    store
        .append("child", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    let deltas = store.load_deltas("child", 0).await.unwrap();
    assert_eq!(deltas[0].run_id, "child-run-1");
    assert_eq!(deltas[0].parent_run_id.as_deref(), Some("parent-run-1"));

    let head = ThreadReader::load(&store, "child").await.unwrap().unwrap();
    assert_eq!(head.thread.parent_thread_id.as_deref(), Some("parent"));
}

/// Verify that actions in a ThreadChangeSet survive append → load_deltas roundtrip.
#[tokio::test]
async fn test_load_deltas_preserves_actions() {
    let store = MemoryStore::new();
    store.create(&Thread::new("t1")).await.unwrap();

    let delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![Arc::new(Message::assistant("hi"))],
        patches: vec![],
        state_actions: vec![SerializedStateAction {
            state_type_name: "MyState".into(),
            base_path: "my_state".into(),
            scope: StateScope::Thread,
            call_id_override: None,
            payload: json!({"DoSomething": 42}),
        }],
        snapshot: None,
    };
    store
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    let deltas = store.load_deltas("t1", 0).await.unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].state_actions.len(), 1);
    assert_eq!(deltas[0].state_actions[0].state_type_name, "MyState");
    assert_eq!(
        deltas[0].state_actions[0].payload,
        json!({"DoSomething": 42})
    );
}

/// Empty delta produces no change but still increments version.
#[tokio::test]
async fn test_append_empty_delta() {
    let store = MemoryStore::new();
    store
        .create(&Thread::new("t1").with_message(Message::user("hi")))
        .await
        .unwrap();

    let empty = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![],
        patches: vec![],
        state_actions: vec![],
        snapshot: None,
    };
    let committed = store
        .append("t1", &empty, VersionPrecondition::Any)
        .await
        .unwrap();
    assert_eq!(committed.version, 1);

    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.version, 1);
    assert_eq!(head.thread.message_count(), 1); // unchanged
}

/// Simulates what `run_stream` does when the frontend sends a state snapshot
/// for an existing thread: the snapshot replaces the current state and is
/// persisted atomically in the UserMessage delta.
#[tokio::test]
async fn frontend_state_replaces_existing_thread_state_in_user_message_delta() {
    let store = MemoryStore::new();

    // 1. Create thread with initial state + a patch
    let thread = Thread::with_initial_state("t1", json!({"counter": 0}));
    store.create(&thread).await.unwrap();
    let patch_delta = ThreadChangeSet {
        run_id: "run-0".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![],
        patches: vec![TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(5))),
        )],
        state_actions: vec![],
        snapshot: None,
    };
    store
        .append("t1", &patch_delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Verify current state: base={"counter":0}, 1 patch → rebuilt={"counter":5}
    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.thread.rebuild_state().unwrap(), json!({"counter": 5}));
    assert_eq!(head.thread.patches.len(), 1);

    // 2. Frontend sends state={"counter":10, "name":"Alice"} along with a user message.
    //    This simulates what run_stream does: include snapshot in UserMessage delta.
    let frontend_state = json!({"counter": 10, "name": "Alice"});
    let user_delta = ThreadChangeSet {
        run_id: "run-1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::UserMessage,
        messages: vec![Arc::new(Message::user("hello"))],
        patches: vec![],
        state_actions: vec![],
        snapshot: Some(frontend_state.clone()),
    };
    store
        .append("t1", &user_delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // 3. Verify: state is fully replaced, patches cleared
    let head = ThreadReader::load(&store, "t1").await.unwrap().unwrap();
    assert_eq!(head.thread.state, frontend_state);
    assert!(head.thread.patches.is_empty());
    assert_eq!(head.thread.rebuild_state().unwrap(), frontend_state);
    // User message was also persisted
    assert!(head.thread.messages.iter().any(|m| m.role == Role::User));
}

#[tokio::test]
async fn mailbox_enqueue_claim_and_ack_roundtrip() {
    let store = MemoryStore::new();
    let entry = MailboxEntryBuilder::queued("entry-mailbox-1", "mailbox-1").build();

    store.enqueue_mailbox_entry(&entry).await.unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 10, "worker-a", 10, 5_000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].status, MailboxEntryStatus::Claimed);
    assert_eq!(claimed[0].claimed_by.as_deref(), Some("worker-a"));

    let claim_token = claimed[0]
        .claim_token
        .clone()
        .expect("claim token should be set");
    store
        .ack_mailbox_entry(&claimed[0].entry_id, &claim_token, 20)
        .await
        .unwrap();

    let loaded = store
        .load_mailbox_entry(&claimed[0].entry_id)
        .await
        .unwrap()
        .expect("entry should still be queryable");
    assert_eq!(loaded.status, MailboxEntryStatus::Accepted);
}

#[tokio::test]
async fn mailbox_cancel_pending_entries_by_mailbox_respects_exclusion() {
    let store = MemoryStore::new();
    let keep = MailboxEntryBuilder::queued("entry-keep", "mailbox-cancel").build();
    let cancel = MailboxEntryBuilder::queued("entry-cancel", "mailbox-cancel").build();
    let other_mailbox = MailboxEntryBuilder::queued("entry-other", "mailbox-other").build();

    store.enqueue_mailbox_entry(&keep).await.unwrap();
    store.enqueue_mailbox_entry(&cancel).await.unwrap();
    store.enqueue_mailbox_entry(&other_mailbox).await.unwrap();

    let cancelled = store
        .cancel_pending_for_mailbox("mailbox-cancel", 50, Some("entry-keep"))
        .await
        .unwrap();

    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].entry_id, "entry-cancel");
    assert_eq!(cancelled[0].status, MailboxEntryStatus::Cancelled);

    let kept = store
        .load_mailbox_entry("entry-keep")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.status, MailboxEntryStatus::Queued);

    let other = store
        .load_mailbox_entry("entry-other")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other.status, MailboxEntryStatus::Queued);
}

#[tokio::test]
async fn mailbox_claim_by_entry_id_ignores_available_at_for_inline_dispatch() {
    let store = MemoryStore::new();
    let entry = MailboxEntryBuilder::queued("entry-inline", "mailbox-inline")
        .with_available_at(i64::MAX as u64)
        .build();
    store.enqueue_mailbox_entry(&entry).await.unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 10, "worker-batch", 10, 5_000)
        .await
        .unwrap();
    assert!(claimed.is_empty());

    let targeted = store
        .claim_mailbox_entry("entry-inline", "worker-inline", 10, 5_000)
        .await
        .unwrap()
        .expect("inline claim should succeed");
    assert_eq!(targeted.status, MailboxEntryStatus::Claimed);
    assert_eq!(targeted.claimed_by.as_deref(), Some("worker-inline"));
}

#[tokio::test]
async fn mailbox_interrupt_bumps_generation_and_supersedes_pending_entries() {
    let store = MemoryStore::new();
    let old_a = MailboxEntryBuilder::queued("entry-old-a", "mailbox-interrupt").build();
    let old_b = MailboxEntryBuilder::queued("entry-old-b", "mailbox-interrupt").build();
    store.enqueue_mailbox_entry(&old_a).await.unwrap();
    store.enqueue_mailbox_entry(&old_b).await.unwrap();

    let interrupted = store
        .interrupt_mailbox("mailbox-interrupt", 50)
        .await
        .unwrap();
    assert_eq!(interrupted.mailbox_state.current_generation, 1);
    assert_eq!(interrupted.superseded_entries.len(), 2);

    let superseded = store
        .load_mailbox_entry("entry-old-a")
        .await
        .unwrap()
        .expect("superseded entry should exist");
    assert_eq!(superseded.status, MailboxEntryStatus::Superseded);

    let next_generation = store
        .ensure_mailbox_state("mailbox-interrupt", 60)
        .await
        .unwrap()
        .current_generation;
    let fresh = MailboxEntryBuilder::queued("entry-fresh", "mailbox-interrupt")
        .with_generation(next_generation)
        .build();
    store.enqueue_mailbox_entry(&fresh).await.unwrap();

    let fresh_loaded = store
        .load_mailbox_entry("entry-fresh")
        .await
        .unwrap()
        .expect("fresh entry should exist");
    assert_eq!(fresh_loaded.generation, 1);
    assert_eq!(fresh_loaded.status, MailboxEntryStatus::Queued);
}

// ---------------------------------------------------------------------------
// nack: claimed → queued with retry_at and error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_nack_returns_entry_to_queued_with_retry_at() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-nack", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-nack", "mailbox-nack").build())
        .await
        .unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer-1", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let token = claimed[0].claim_token.clone().unwrap();
    assert_eq!(claimed[0].attempt_count, 1);

    store
        .nack_mailbox_entry("entry-nack", &token, 1000, "transient error", 10)
        .await
        .unwrap();

    let entry = store
        .load_mailbox_entry("entry-nack")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, MailboxEntryStatus::Queued);
    assert_eq!(entry.available_at, 1000);
    assert_eq!(entry.last_error.as_deref(), Some("transient error"));
    assert!(entry.claim_token.is_none());
    assert!(entry.claimed_by.is_none());

    // Not claimable before retry_at
    let before_retry = store
        .claim_mailbox_entries(None, 1, "consumer-2", 500, 5000)
        .await
        .unwrap();
    assert_eq!(before_retry.len(), 0);

    // Claimable at retry_at
    let at_retry = store
        .claim_mailbox_entries(None, 1, "consumer-2", 1000, 5000)
        .await
        .unwrap();
    assert_eq!(at_retry.len(), 1);
    assert_eq!(at_retry[0].attempt_count, 2);
}

#[tokio::test]
async fn mailbox_nack_with_wrong_token_returns_claim_conflict() {
    let store = MemoryStore::new();
    store
        .ensure_mailbox_state("mailbox-nack-conflict", 1)
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-nc", "mailbox-nack-conflict").build(),
        )
        .await
        .unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer-1", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let result = store
        .nack_mailbox_entry("entry-nc", "wrong-token", 1000, "err", 10)
        .await;
    assert!(matches!(result, Err(MailboxStoreError::ClaimConflict(_))));
}

// ---------------------------------------------------------------------------
// dead_letter: claimed → dead_letter with error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_dead_letter_marks_entry_terminal() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-dl", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-dl", "mailbox-dl").build())
        .await
        .unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer-1", 10, 5000)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store
        .dead_letter_mailbox_entry("entry-dl", &token, "permanent failure", 10)
        .await
        .unwrap();

    let entry = store.load_mailbox_entry("entry-dl").await.unwrap().unwrap();
    assert_eq!(entry.status, MailboxEntryStatus::DeadLetter);
    assert_eq!(entry.last_error.as_deref(), Some("permanent failure"));
    assert!(entry.claim_token.is_none());
    assert!(entry.status.is_terminal());
}

#[tokio::test]
async fn mailbox_dead_letter_with_wrong_token_returns_claim_conflict() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-dl2", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-dl2", "mailbox-dl2").build())
        .await
        .unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer-1", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let result = store
        .dead_letter_mailbox_entry("entry-dl2", "wrong-token", "err", 10)
        .await;
    assert!(matches!(result, Err(MailboxStoreError::ClaimConflict(_))));
}

// ---------------------------------------------------------------------------
// purge_terminal: GC for terminal entries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_purge_terminal_removes_old_terminal_entries_only() {
    let store = MemoryStore::new();
    store
        .ensure_mailbox_state("mailbox-purge", 1)
        .await
        .unwrap();

    // Create entries with different terminal statuses and timestamps
    let accepted = MailboxEntryBuilder::queued("entry-accepted", "mailbox-purge")
        .with_status(MailboxEntryStatus::Accepted)
        .with_updated_at(100)
        .build();
    store.enqueue_mailbox_entry(&accepted).await.unwrap();

    let cancelled = MailboxEntryBuilder::queued("entry-cancelled", "mailbox-purge")
        .with_status(MailboxEntryStatus::Cancelled)
        .with_updated_at(200)
        .build();
    store.enqueue_mailbox_entry(&cancelled).await.unwrap();

    let recent_dl = MailboxEntryBuilder::queued("entry-recent-dl", "mailbox-purge")
        .with_status(MailboxEntryStatus::DeadLetter)
        .with_updated_at(900)
        .build();
    store.enqueue_mailbox_entry(&recent_dl).await.unwrap();

    let queued = MailboxEntryBuilder::queued("entry-queued", "mailbox-purge").build();
    store.enqueue_mailbox_entry(&queued).await.unwrap();

    // Purge with cutoff=500: should remove accepted(100), cancelled(200) but not recent_dl(900) or queued
    let purged = store.purge_terminal_mailbox_entries(500).await.unwrap();
    assert_eq!(purged, 2);

    assert!(store
        .load_mailbox_entry("entry-accepted")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .load_mailbox_entry("entry-cancelled")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .load_mailbox_entry("entry-recent-dl")
        .await
        .unwrap()
        .is_some());
    assert!(store
        .load_mailbox_entry("entry-queued")
        .await
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------
// Duplicate entry_id rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_enqueue_duplicate_entry_id_returns_already_exists() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-dup", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-dup", "mailbox-dup").build())
        .await
        .unwrap();

    let result = store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-dup", "mailbox-dup").build())
        .await;
    assert!(matches!(result, Err(MailboxStoreError::AlreadyExists(_))));
}

#[tokio::test]
async fn mailbox_enqueue_duplicate_dedupe_key_in_same_mailbox_returns_already_exists() {
    let store = MemoryStore::new();
    store
        .ensure_mailbox_state("mailbox-dup-dedupe", 1)
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-dup-dedupe-1", "mailbox-dup-dedupe")
                .with_dedupe_key("dup-key")
                .build(),
        )
        .await
        .unwrap();

    let result = store
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-dup-dedupe-2", "mailbox-dup-dedupe")
                .with_dedupe_key("dup-key")
                .build(),
        )
        .await;
    assert!(matches!(result, Err(MailboxStoreError::AlreadyExists(_))));
}

// ---------------------------------------------------------------------------
// Generation mismatch on enqueue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_enqueue_with_stale_generation_returns_mismatch() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-gen", 1).await.unwrap();

    // Bump generation via interrupt
    store.interrupt_mailbox("mailbox-gen", 10).await.unwrap();

    // Try to enqueue with generation 0 (stale)
    let entry = MailboxEntryBuilder::queued("entry-stale", "mailbox-gen").build();
    let result = store.enqueue_mailbox_entry(&entry).await;
    assert!(matches!(
        result,
        Err(MailboxStoreError::GenerationMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// claim_mailbox_entries with mailbox_id filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_claim_entries_filters_by_mailbox_id() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-a", 1).await.unwrap();
    store.ensure_mailbox_state("mailbox-b", 1).await.unwrap();

    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-a1", "mailbox-a").build())
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-b1", "mailbox-b").build())
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-a2", "mailbox-a").build())
        .await
        .unwrap();

    // Claim only from mailbox-a — exclusive: only one per mailbox
    let claimed_a = store
        .claim_mailbox_entries(Some("mailbox-a"), 10, "consumer-1", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed_a.len(), 1, "exclusive claim: one per mailbox");
    assert_eq!(claimed_a[0].mailbox_id, "mailbox-a");
    let token_a = claimed_a[0].claim_token.clone().unwrap();

    // Claim from mailbox-b — independent mailbox, should work
    let claimed_b = store
        .claim_mailbox_entries(Some("mailbox-b"), 10, "consumer-1", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed_b.len(), 1);
    assert_eq!(claimed_b[0].mailbox_id, "mailbox-b");

    // Claim all (no filter) — both mailboxes have active claims
    let claimed_all = store
        .claim_mailbox_entries(None, 10, "consumer-2", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed_all.len(), 0);

    // Ack the mailbox-a claim, then second entry becomes claimable
    store
        .ack_mailbox_entry("entry-a1", &token_a, 20)
        .await
        .unwrap();
    let claimed_a2 = store
        .claim_mailbox_entries(Some("mailbox-a"), 10, "consumer-1", 21, 5000)
        .await
        .unwrap();
    assert_eq!(claimed_a2.len(), 1);
    assert_eq!(claimed_a2[0].entry_id, "entry-a2");
}

// ---------------------------------------------------------------------------
// Lease expiry: expired claimed entries are re-claimable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_expired_lease_allows_reclaim() {
    let store = MemoryStore::new();
    store
        .ensure_mailbox_state("mailbox-lease", 1)
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-lease", "mailbox-lease").build())
        .await
        .unwrap();

    // Claim with short lease (expires at now + 100 = 110)
    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer-1", 10, 100)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].lease_until, Some(110));
    let token1 = claimed[0].claim_token.clone().unwrap();

    // Not claimable before lease expires
    let no_claim = store
        .claim_mailbox_entries(None, 1, "consumer-2", 50, 100)
        .await
        .unwrap();
    assert_eq!(no_claim.len(), 0);

    // Claimable after lease expires
    let reclaimed = store
        .claim_mailbox_entries(None, 1, "consumer-2", 120, 100)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    let token2 = reclaimed[0].claim_token.clone().unwrap();
    assert_ne!(token1, token2);
    assert_eq!(reclaimed[0].claimed_by.as_deref(), Some("consumer-2"));
    assert_eq!(reclaimed[0].attempt_count, 2);
}

// ---------------------------------------------------------------------------
// supersede_mailbox_entry: single entry supersede
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_supersede_entry_marks_as_superseded() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-sup", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-sup", "mailbox-sup").build())
        .await
        .unwrap();

    let result = store
        .supersede_mailbox_entry("entry-sup", 10, "replaced by newer")
        .await
        .unwrap();
    assert!(result.is_some());
    let entry = result.unwrap();
    assert_eq!(entry.status, MailboxEntryStatus::Superseded);
    assert_eq!(entry.last_error.as_deref(), Some("replaced by newer"));
    assert!(entry.status.is_terminal());

    // Superseding terminal entry returns it unchanged
    let again = store
        .supersede_mailbox_entry("entry-sup", 20, "another reason")
        .await
        .unwrap();
    assert!(again.is_some());
    assert_eq!(again.unwrap().status, MailboxEntryStatus::Superseded);
}

// ---------------------------------------------------------------------------
// cancel_mailbox_entry: various statuses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_cancel_entry_for_nonexistent_returns_none() {
    let store = MemoryStore::new();
    let result = store.cancel_mailbox_entry("nonexistent", 10).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn mailbox_cancel_terminal_entry_returns_unchanged() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-ct", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-ct", "mailbox-ct").build())
        .await
        .unwrap();

    // Cancel it first
    store.cancel_mailbox_entry("entry-ct", 10).await.unwrap();
    // Cancel again — returns the already-cancelled entry
    let result = store
        .cancel_mailbox_entry("entry-ct", 20)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, MailboxEntryStatus::Cancelled);
    assert_eq!(result.updated_at, 10); // not updated to 20
}

// ---------------------------------------------------------------------------
// Priority ordering in claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_claim_respects_priority_ordering() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-prio", 1).await.unwrap();

    let low = MailboxEntryBuilder::queued("entry-low", "mailbox-prio")
        .with_priority(0)
        .with_created_at(1)
        .build();
    store.enqueue_mailbox_entry(&low).await.unwrap();

    let high = MailboxEntryBuilder::queued("entry-high", "mailbox-prio")
        .with_priority(10)
        .with_created_at(2)
        .build();
    store.enqueue_mailbox_entry(&high).await.unwrap();

    let medium = MailboxEntryBuilder::queued("entry-med", "mailbox-prio")
        .with_priority(5)
        .with_created_at(3)
        .build();
    store.enqueue_mailbox_entry(&medium).await.unwrap();

    // Exclusive claim: only the highest-priority entry should be claimed
    let claimed = store
        .claim_mailbox_entries(None, 10, "consumer", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1, "exclusive claim: one per mailbox");
    assert_eq!(claimed[0].entry_id, "entry-high", "highest priority wins");
}

// ---------------------------------------------------------------------------
// ensure_mailbox_state: idempotent creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_ensure_state_is_idempotent() {
    let store = MemoryStore::new();
    let state1 = store
        .ensure_mailbox_state("mailbox-ensure", 10)
        .await
        .unwrap();
    assert_eq!(state1.current_generation, 0);
    assert_eq!(state1.updated_at, 10);

    let state2 = store
        .ensure_mailbox_state("mailbox-ensure", 20)
        .await
        .unwrap();
    assert_eq!(state2.current_generation, 0);
    assert_eq!(state2.updated_at, 20);
}

// ---------------------------------------------------------------------------
// ack_mailbox_entry: token mismatch and not-found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_ack_with_wrong_token_returns_claim_conflict() {
    let store = MemoryStore::new();
    store
        .ensure_mailbox_state("mailbox-ack-c", 1)
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-ack-c", "mailbox-ack-c").build())
        .await
        .unwrap();

    let claimed = store
        .claim_mailbox_entries(None, 1, "consumer", 10, 5000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let result = store
        .ack_mailbox_entry("entry-ack-c", "wrong-token", 10)
        .await;
    assert!(matches!(result, Err(MailboxStoreError::ClaimConflict(_))));
}

#[tokio::test]
async fn mailbox_ack_nonexistent_entry_returns_not_found() {
    let store = MemoryStore::new();
    let result = store
        .ack_mailbox_entry("nonexistent", "any-token", 10)
        .await;
    assert!(matches!(result, Err(MailboxStoreError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// list_mailbox_entries: pagination and filtering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_list_entries_filters_and_paginates() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("mailbox-list", 1).await.unwrap();

    for i in 0..5 {
        let entry = MailboxEntryBuilder::queued(format!("entry-list-{i}"), "mailbox-list")
            .with_created_at(i as u64 + 1)
            .build();
        store.enqueue_mailbox_entry(&entry).await.unwrap();
    }

    // Cancel one to create mixed statuses
    store
        .cancel_mailbox_entry("entry-list-2", 10)
        .await
        .unwrap();

    // List all for this mailbox
    let all = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-list".to_string()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(all.total, 5);

    // List only queued
    let queued = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-list".to_string()),
            status: Some(MailboxEntryStatus::Queued),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(queued.total, 4);

    // List only cancelled
    let cancelled = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-list".to_string()),
            status: Some(MailboxEntryStatus::Cancelled),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(cancelled.total, 1);

    // Pagination: limit 2, offset 0
    let page1 = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-list".to_string()),
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page1.items.len(), 2);
    assert!(page1.has_more);

    // Pagination: limit 2, offset 4
    let page_last = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-list".to_string()),
            limit: 2,
            offset: 4,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page_last.items.len(), 1);
    assert!(!page_last.has_more);
}

#[tokio::test]
async fn mailbox_list_entries_filters_by_origin() {
    let store = MemoryStore::new();

    store
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-ext", "mailbox-origin")
                .with_origin(MailboxEntryOrigin::External)
                .build(),
        )
        .await
        .unwrap();
    store
        .enqueue_mailbox_entry(
            &MailboxEntryBuilder::queued("entry-int", "mailbox-origin")
                .with_origin(MailboxEntryOrigin::Internal)
                .build(),
        )
        .await
        .unwrap();

    let external = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-origin".to_string()),
            origin: Some(MailboxEntryOrigin::External),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(external.total, 1);
    assert_eq!(external.items[0].entry_id, "entry-ext");

    let internal = store
        .list_mailbox_entries(&MailboxQuery {
            mailbox_id: Some("mailbox-origin".to_string()),
            origin: Some(MailboxEntryOrigin::Internal),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(internal.total, 1);
    assert_eq!(internal.items[0].entry_id, "entry-int");
}

// ---------------------------------------------------------------------------
// Mailbox-level exclusive claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_exclusive_claim_prevents_concurrent_claims_on_same_mailbox() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-1", 1).await.unwrap();

    // Enqueue two entries for the same mailbox
    let entry_a = MailboxEntryBuilder::queued("entry-a", "thread-1").build();
    let entry_b = MailboxEntryBuilder::queued("entry-b", "thread-1").build();
    store.enqueue_mailbox_entry(&entry_a).await.unwrap();
    store.enqueue_mailbox_entry(&entry_b).await.unwrap();

    // Claim first entry
    let claimed = store
        .claim_mailbox_entries(Some("thread-1"), 10, "worker-1", 10, 30_000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1, "should only claim one entry per mailbox");
    assert_eq!(claimed[0].entry_id, "entry-a");

    // Try to claim more — should get none because mailbox has active claim
    let claimed2 = store
        .claim_mailbox_entries(Some("thread-1"), 10, "worker-2", 11, 30_000)
        .await
        .unwrap();
    assert_eq!(claimed2.len(), 0, "second claim should be blocked");

    // claim_mailbox_entry by ID should also be blocked
    let by_id = store
        .claim_mailbox_entry("entry-b", "worker-2", 12, 30_000)
        .await
        .unwrap();
    assert!(by_id.is_none(), "direct claim should be blocked too");
}

#[tokio::test]
async fn mailbox_exclusive_claim_allows_claim_after_ack() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-2", 1).await.unwrap();

    let entry_a = MailboxEntryBuilder::queued("entry-2a", "thread-2").build();
    let entry_b = MailboxEntryBuilder::queued("entry-2b", "thread-2").build();
    store.enqueue_mailbox_entry(&entry_a).await.unwrap();
    store.enqueue_mailbox_entry(&entry_b).await.unwrap();

    // Claim and ack first entry
    let claimed = store
        .claim_mailbox_entry("entry-2a", "worker-1", 10, 30_000)
        .await
        .unwrap()
        .unwrap();
    let token = claimed.claim_token.unwrap();
    store
        .ack_mailbox_entry("entry-2a", &token, 20)
        .await
        .unwrap();

    // Now second entry should be claimable
    let claimed2 = store
        .claim_mailbox_entry("entry-2b", "worker-1", 21, 30_000)
        .await
        .unwrap();
    assert!(claimed2.is_some(), "should be claimable after ack");
}

#[tokio::test]
async fn mailbox_exclusive_claim_allows_claim_after_lease_expires() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-3", 1).await.unwrap();

    let entry_a = MailboxEntryBuilder::queued("entry-3a", "thread-3").build();
    let entry_b = MailboxEntryBuilder::queued("entry-3b", "thread-3").build();
    store.enqueue_mailbox_entry(&entry_a).await.unwrap();
    store.enqueue_mailbox_entry(&entry_b).await.unwrap();

    // Claim with short lease (expires at 10 + 100 = 110)
    store
        .claim_mailbox_entry("entry-3a", "worker-1", 10, 100)
        .await
        .unwrap();

    // Blocked before expiry
    let blocked = store
        .claim_mailbox_entry("entry-3b", "worker-2", 50, 30_000)
        .await
        .unwrap();
    assert!(blocked.is_none());

    // Allowed after lease expires
    let allowed = store
        .claim_mailbox_entry("entry-3b", "worker-2", 120, 30_000)
        .await
        .unwrap();
    assert!(allowed.is_some());
}

#[tokio::test]
async fn mailbox_exclusive_claim_different_mailboxes_are_independent() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-x", 1).await.unwrap();
    store.ensure_mailbox_state("thread-y", 1).await.unwrap();

    let entry_x = MailboxEntryBuilder::queued("entry-x", "thread-x").build();
    let entry_y = MailboxEntryBuilder::queued("entry-y", "thread-y").build();
    store.enqueue_mailbox_entry(&entry_x).await.unwrap();
    store.enqueue_mailbox_entry(&entry_y).await.unwrap();

    // Claim on thread-x
    let claimed_x = store
        .claim_mailbox_entry("entry-x", "worker-1", 10, 30_000)
        .await
        .unwrap();
    assert!(claimed_x.is_some());

    // Claim on thread-y should still work
    let claimed_y = store
        .claim_mailbox_entry("entry-y", "worker-1", 11, 30_000)
        .await
        .unwrap();
    assert!(claimed_y.is_some(), "different mailboxes are independent");
}

#[tokio::test]
async fn mailbox_batch_claim_returns_at_most_one_per_mailbox() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-m1", 1).await.unwrap();
    store.ensure_mailbox_state("thread-m2", 1).await.unwrap();

    // Two entries per mailbox
    for (i, mid) in [
        ("1", "thread-m1"),
        ("2", "thread-m1"),
        ("3", "thread-m2"),
        ("4", "thread-m2"),
    ]
    .iter()
    {
        store
            .enqueue_mailbox_entry(
                &MailboxEntryBuilder::queued(format!("entry-batch-{i}"), *mid).build(),
            )
            .await
            .unwrap();
    }

    let claimed = store
        .claim_mailbox_entries(None, 10, "worker-1", 10, 30_000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2, "one per mailbox");
    let mailbox_ids: Vec<&str> = claimed.iter().map(|e| e.mailbox_id.as_str()).collect();
    assert!(mailbox_ids.contains(&"thread-m1"));
    assert!(mailbox_ids.contains(&"thread-m2"));
}

// ---------------------------------------------------------------------------
// extend_lease
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_extend_lease_extends_active_claim() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-el", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-el", "thread-el").build())
        .await
        .unwrap();

    // Claim with lease expiring at 10 + 100 = 110
    let claimed = store
        .claim_mailbox_entry("entry-el", "worker-1", 10, 100)
        .await
        .unwrap()
        .unwrap();
    let token = claimed.claim_token.unwrap();

    // Extend at now=50, extension_ms=200 → new lease_until = 250
    let extended = store
        .extend_lease("entry-el", &token, 200, 50)
        .await
        .unwrap();
    assert!(extended);

    let loaded = store.load_mailbox_entry("entry-el").await.unwrap().unwrap();
    assert_eq!(loaded.lease_until, Some(250));
}

#[tokio::test]
async fn mailbox_extend_lease_rejects_wrong_token() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-elt", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-elt", "thread-elt").build())
        .await
        .unwrap();

    store
        .claim_mailbox_entry("entry-elt", "worker-1", 10, 100)
        .await
        .unwrap();

    let result = store
        .extend_lease("entry-elt", "wrong-token", 200, 50)
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn mailbox_extend_lease_rejects_non_claimed() {
    let store = MemoryStore::new();
    store.ensure_mailbox_state("thread-elnc", 1).await.unwrap();
    store
        .enqueue_mailbox_entry(&MailboxEntryBuilder::queued("entry-elnc", "thread-elnc").build())
        .await
        .unwrap();

    // Entry is Queued, not Claimed
    let result = store
        .extend_lease("entry-elnc", "any-token", 200, 50)
        .await
        .unwrap();
    assert!(!result);
}

// ========================================================================
// TaskBoard tests
// ========================================================================

fn make_task(id: &str) -> Task {
    Task {
        task_id: id.to_string(),
        subject: format!("Task {id}"),
        description: None,
        owner: None,
        status: TaskStatus::Pending,
        blocks: vec![],
        blocked_by: vec![],
        team_id: Some("team-1".to_string()),
        metadata: None,
        created_at: 100,
        updated_at: 100,
        created_by: Some("lead".to_string()),
    }
}

#[tokio::test]
async fn task_create_and_load() {
    let store = MemoryStore::new();
    let task = make_task("t1");
    store.create_task(&task).await.unwrap();

    let loaded = store.load_task("t1").await.unwrap().unwrap();
    assert_eq!(loaded.task_id, "t1");
    assert_eq!(loaded.subject, "Task t1");
}

#[tokio::test]
async fn task_create_duplicate_fails() {
    let store = MemoryStore::new();
    let task = make_task("t1");
    store.create_task(&task).await.unwrap();
    let err = store.create_task(&task).await.unwrap_err();
    assert!(matches!(err, TaskStoreError::AlreadyExists(_)));
}

#[tokio::test]
async fn task_update_partial_fields() {
    use tirea_contract::storage::TaskUpdate;
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();

    let updated = store
        .update_task(
            "t1",
            &TaskUpdate {
                subject: Some("New subject".to_string()),
                description: Some(Some("desc".to_string())),
                ..Default::default()
            },
            200,
        )
        .await
        .unwrap();
    assert_eq!(updated.subject, "New subject");
    assert_eq!(updated.description.as_deref(), Some("desc"));
    assert_eq!(updated.updated_at, 200);
    // Unmodified fields preserved
    assert_eq!(updated.status, TaskStatus::Pending);
}

#[tokio::test]
async fn task_claim_cas_succeeds_when_unowned() {
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();

    let claimed = store.claim_task("t1", "agent-a", 200).await.unwrap();
    assert_eq!(claimed.owner.as_deref(), Some("agent-a"));
    assert_eq!(claimed.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn task_claim_cas_fails_when_owned() {
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();
    store.claim_task("t1", "agent-a", 200).await.unwrap();

    let err = store.claim_task("t1", "agent-b", 300).await.unwrap_err();
    assert!(matches!(err, TaskStoreError::ClaimConflict(_)));
}

#[tokio::test]
async fn task_claim_blocked_by_incomplete_deps() {
    let store = MemoryStore::new();
    let mut dep = make_task("dep-1");
    dep.blocks = vec!["t1".to_string()];
    store.create_task(&dep).await.unwrap();

    let mut task = make_task("t1");
    task.blocked_by = vec!["dep-1".to_string()];
    store.create_task(&task).await.unwrap();

    let err = store.claim_task("t1", "agent-a", 200).await.unwrap_err();
    assert!(matches!(err, TaskStoreError::Blocked(_)));
}

#[tokio::test]
async fn task_claim_succeeds_after_dep_completed() {
    let store = MemoryStore::new();
    let mut dep = make_task("dep-1");
    dep.blocks = vec!["t1".to_string()];
    store.create_task(&dep).await.unwrap();

    let mut task = make_task("t1");
    task.blocked_by = vec!["dep-1".to_string()];
    store.create_task(&task).await.unwrap();

    // Complete the dependency
    store.complete_task("dep-1", 150).await.unwrap();

    // Now claim should succeed (blocked_by was cleared by complete_task)
    let claimed = store.claim_task("t1", "agent-a", 200).await.unwrap();
    assert_eq!(claimed.owner.as_deref(), Some("agent-a"));
}

#[tokio::test]
async fn task_assign_overrides_existing_owner() {
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();
    store.claim_task("t1", "agent-a", 200).await.unwrap();

    let assigned = store.assign_task("t1", "agent-b", 300).await.unwrap();
    assert_eq!(assigned.owner.as_deref(), Some("agent-b"));
}

#[tokio::test]
async fn task_complete_clears_downstream_blocked_by() {
    let store = MemoryStore::new();
    let mut upstream = make_task("up");
    upstream.blocks = vec!["down".to_string()];
    store.create_task(&upstream).await.unwrap();

    let mut downstream = make_task("down");
    downstream.blocked_by = vec!["up".to_string()];
    store.create_task(&downstream).await.unwrap();

    store.claim_task("up", "agent-a", 200).await.unwrap();
    let completed = store.complete_task("up", 300).await.unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);

    let down = store.load_task("down").await.unwrap().unwrap();
    assert!(down.blocked_by.is_empty());
}

#[tokio::test]
async fn task_delete() {
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();
    store.delete_task("t1").await.unwrap();
    assert!(store.load_task("t1").await.unwrap().is_none());
}

#[tokio::test]
async fn task_delete_not_found() {
    let store = MemoryStore::new();
    let err = store.delete_task("nope").await.unwrap_err();
    assert!(matches!(err, TaskStoreError::NotFound(_)));
}

#[tokio::test]
async fn task_list_filters_by_team_and_status() {
    let store = MemoryStore::new();
    store.create_task(&make_task("t1")).await.unwrap();
    let mut t2 = make_task("t2");
    t2.status = TaskStatus::InProgress;
    t2.owner = Some("agent-a".to_string());
    store.create_task(&t2).await.unwrap();
    let mut t3 = make_task("t3");
    t3.team_id = Some("team-2".to_string());
    store.create_task(&t3).await.unwrap();

    // Filter by team
    let page = store
        .list_tasks(&TaskQuery {
            team_id: Some("team-1".to_string()),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2);

    // Filter by status
    let page = store
        .list_tasks(&TaskQuery {
            status: Some(TaskStatus::Pending),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2); // t1 and t3

    // Filter by owner
    let page = store
        .list_tasks(&TaskQuery {
            owner: Some("agent-a".to_string()),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].task_id, "t2");
}

/// Chain A→B→C: completing A unblocks B, completing B unblocks C.
#[tokio::test]
async fn task_dep_chain_a_b_c() {
    let store = MemoryStore::new();

    let mut a = make_task("a");
    a.blocks = vec!["b".to_string()];
    store.create_task(&a).await.unwrap();

    let mut b = make_task("b");
    b.blocked_by = vec!["a".to_string()];
    b.blocks = vec!["c".to_string()];
    store.create_task(&b).await.unwrap();

    let mut c = make_task("c");
    c.blocked_by = vec!["b".to_string()];
    store.create_task(&c).await.unwrap();

    // C is blocked by B which is blocked by A.
    assert!(store.claim_task("c", "x", 10).await.is_err());
    assert!(store.claim_task("b", "x", 10).await.is_err());

    // Complete A → B becomes claimable, C still blocked.
    store.complete_task("a", 20).await.unwrap();
    store.claim_task("b", "x", 30).await.unwrap();
    assert!(store.claim_task("c", "y", 30).await.is_err());

    // Complete B → C becomes claimable.
    store.complete_task("b", 40).await.unwrap();
    let claimed_c = store.claim_task("c", "y", 50).await.unwrap();
    assert_eq!(claimed_c.owner.as_deref(), Some("y"));
}

/// Diamond: D depends on both B and C, which both depend on A.
#[tokio::test]
async fn task_dep_diamond() {
    let store = MemoryStore::new();

    let a = make_task("a");
    store.create_task(&a).await.unwrap();

    let mut b = make_task("b");
    b.blocked_by = vec!["a".to_string()];
    store.create_task(&b).await.unwrap();

    let mut c = make_task("c");
    c.blocked_by = vec!["a".to_string()];
    store.create_task(&c).await.unwrap();

    let mut d = make_task("d");
    d.blocked_by = vec!["b".to_string(), "c".to_string()];
    store.create_task(&d).await.unwrap();

    // Complete A → unblocks B and C.
    store.complete_task("a", 10).await.unwrap();
    let b_loaded = store.load_task("b").await.unwrap().unwrap();
    assert!(b_loaded.blocked_by.is_empty());
    let c_loaded = store.load_task("c").await.unwrap().unwrap();
    assert!(c_loaded.blocked_by.is_empty());

    // D still blocked (needs both B and C complete).
    assert!(store.claim_task("d", "x", 20).await.is_err());

    // Complete B only → D still blocked by C.
    store.claim_task("b", "x", 20).await.unwrap();
    store.complete_task("b", 30).await.unwrap();
    let d_loaded = store.load_task("d").await.unwrap().unwrap();
    assert_eq!(d_loaded.blocked_by, vec!["c".to_string()]);
    assert!(store.claim_task("d", "x", 30).await.is_err());

    // Complete C → D fully unblocked.
    store.claim_task("c", "y", 30).await.unwrap();
    store.complete_task("c", 40).await.unwrap();
    let d_final = store.claim_task("d", "z", 50).await.unwrap();
    assert!(d_final.blocked_by.is_empty());
    assert_eq!(d_final.owner.as_deref(), Some("z"));
}

/// Claim should fail when a dependency task_id doesn't exist in the store.
#[tokio::test]
async fn task_claim_fails_when_dep_missing() {
    let store = MemoryStore::new();
    let mut task = make_task("t1");
    task.blocked_by = vec!["nonexistent".to_string()];
    store.create_task(&task).await.unwrap();

    let err = store.claim_task("t1", "agent", 10).await.unwrap_err();
    assert!(matches!(err, TaskStoreError::Blocked(_)));
}

/// Completing a task with no downstream dependents should have no side effects.
#[tokio::test]
async fn task_complete_no_downstream_is_noop() {
    let store = MemoryStore::new();
    store.create_task(&make_task("solo")).await.unwrap();

    let other = make_task("other");
    store.create_task(&other).await.unwrap();

    store.complete_task("solo", 20).await.unwrap();

    // Other task is untouched.
    let loaded = store.load_task("other").await.unwrap().unwrap();
    assert_eq!(loaded.updated_at, 100); // original timestamp
}

/// Partial deps: task has 2 deps, only 1 completed → still blocked.
#[tokio::test]
async fn task_claim_partial_deps_still_blocked() {
    let store = MemoryStore::new();

    store.create_task(&make_task("d1")).await.unwrap();
    store.create_task(&make_task("d2")).await.unwrap();

    let mut task = make_task("t1");
    task.blocked_by = vec!["d1".to_string(), "d2".to_string()];
    store.create_task(&task).await.unwrap();

    // Complete only d1.
    store.complete_task("d1", 20).await.unwrap();

    // Still blocked by d2.
    let err = store.claim_task("t1", "agent", 30).await.unwrap_err();
    assert!(matches!(err, TaskStoreError::Blocked(_)));

    // Complete d2 → now claimable.
    store.complete_task("d2", 40).await.unwrap();
    let claimed = store.claim_task("t1", "agent", 50).await.unwrap();
    assert_eq!(claimed.owner.as_deref(), Some("agent"));
}
