use serde_json::json;
use std::sync::Arc;
use tirea_contract::runtime::state::SerializedAction;
use tirea_contract::storage::{
    ThreadReader, ThreadStore, ThreadStoreError, ThreadSync, ThreadWriter, VersionPrecondition,
};
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
        reason,
        messages: vec![Arc::new(Message::assistant("hello"))],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::RunFinished,
        messages: vec![],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![
            Arc::new(Message::assistant_with_tool_calls(
                "Calculating...",
                vec![tool_call],
            )),
            Arc::new(Message::tool("call_42", r#"{"answer": 42}"#)),
        ],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![
            Arc::new(Message::assistant_with_tool_calls("multi-tool", calls)),
            Arc::new(Message::tool("call_a", "search result")),
            Arc::new(Message::tool("call_b", "fetch result")),
        ],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::UserMessage,
        messages: vec![Arc::new(Message::user("What is 2+2?"))],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("2+2 = 4"))],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![Arc::new(Message::tool("call-1", "4"))],
        patches: vec![patch],
        actions: vec![],
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
        reason: CheckpointReason::RunFinished,
        messages: vec![Arc::new(Message::assistant("The answer is 4."))],
        patches: vec![],
        actions: vec![],
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
            reason: CheckpointReason::UserMessage,
            messages: vec![Arc::new(Message::user("inc"))],
            patches: vec![TrackedPatch::new(
                Patch::new().with_op(Op::increment(path!("count"), 1)),
            )],
            actions: vec![],
            snapshot: None,
        },
        ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant("done"))],
            patches: vec![TrackedPatch::new(
                Patch::new().with_op(Op::increment(path!("count"), 1)),
            )],
            actions: vec![],
            snapshot: None,
        },
        ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            reason: CheckpointReason::RunFinished,
            messages: vec![],
            patches: vec![],
            actions: vec![],
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
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant(format!("msg-{i}")))],
            patches: vec![],
            actions: vec![],
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
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![],
        patches: vec![patch],
        actions: vec![],
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
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("sub-agent reply"))],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![Arc::new(Message::assistant("hi"))],
        patches: vec![],
        actions: vec![SerializedAction {
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
    assert_eq!(deltas[0].actions.len(), 1);
    assert_eq!(deltas[0].actions[0].state_type_name, "MyState");
    assert_eq!(deltas[0].actions[0].payload, json!({"DoSomething": 42}));
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
        reason: CheckpointReason::RunFinished,
        messages: vec![],
        patches: vec![],
        actions: vec![],
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
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![],
        patches: vec![TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(5))),
        )],
        actions: vec![],
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
        reason: CheckpointReason::UserMessage,
        messages: vec![Arc::new(Message::user("hello"))],
        patches: vec![],
        actions: vec![],
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
