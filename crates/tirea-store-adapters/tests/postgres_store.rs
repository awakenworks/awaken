//! Integration tests for PostgresStore using testcontainers.
//!
//! Requires Docker. Run with:
//! ```bash
//! cargo test --package tirea-store-adapters --features postgres --test postgres_store -- --nocapture
//! ```

#![cfg(feature = "postgres")]

use serde_json::json;
use std::sync::Arc;
use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers_modules::postgres::Postgres;
use tirea_contract::storage::{
    MailboxEntry, MailboxEntryStatus, MailboxReader, MailboxWriter, RunOrigin, RunQuery, RunReader,
    RunRecord, RunStatus, RunWriter, ThreadReader, ThreadStoreError, ThreadWriter,
    VersionPrecondition,
};
use tirea_contract::thread::ThreadChangeSet;
use tirea_contract::{CheckpointReason, Message, MessageQuery, RunRequest, Thread, ToolCall};
use tirea_store_adapters::PostgresStore;

async fn start_postgres() -> Option<(testcontainers::ContainerAsync<Postgres>, String)> {
    let container = match Postgres::default()
        .with_env_var("POSTGRES_DB", "tirea_test")
        .with_env_var("POSTGRES_USER", "tirea")
        .with_env_var("POSTGRES_PASSWORD", "tirea")
        .start()
        .await
    {
        Ok(container) => container,
        Err(_) => {
            return None;
        }
    };
    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("failed to get port");
    let url = format!("postgres://tirea:tirea@{host}:{port}/tirea_test");
    Some((container, url))
}

async fn make_store(database_url: &str) -> PostgresStore {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("failed to connect to Postgres");
    let store = PostgresStore::new(pool);
    store.ensure_table().await.expect("failed to create tables");
    store
}

async fn make_store_without_ensure(database_url: &str) -> PostgresStore {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("failed to connect to Postgres");
    PostgresStore::new(pool)
}

fn mailbox_entry(run_id: &str, thread_id: &str) -> MailboxEntry {
    MailboxEntry {
        entry_id: format!("entry-{run_id}"),
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        agent_id: "agent".to_string(),
        generation: 0,
        status: MailboxEntryStatus::Queued,
        request: RunRequest {
            agent_id: "agent".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some(run_id.to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user("hello")],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        },
        dedupe_key: None,
        available_at: 1,
        attempt_count: 0,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        accepted_run_id: None,
        created_at: 1,
        updated_at: 1,
    }
}

// ========================================================================
// Basic round-trip
// ========================================================================

#[tokio::test]
async fn test_save_load_roundtrip() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let thread = Thread::new("t1").with_message(Message::user("hello"));
    store.save(&thread).await.unwrap();

    let loaded = store.load_thread("t1").await.unwrap().unwrap();
    assert_eq!(loaded.id, "t1");
    assert_eq!(loaded.message_count(), 1);
    assert_eq!(loaded.messages[0].content, "hello");
}

#[tokio::test]
async fn test_auto_initializes_schema_on_first_thread_access() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store_without_ensure(&url).await;

    let missing = store.load_thread("missing-thread").await.unwrap();
    assert!(missing.is_none(), "read access should bootstrap schema");

    let thread = Thread::new("auto-init-thread").with_message(Message::user("hello"));
    store
        .save(&thread)
        .await
        .expect("save should auto-create tables");

    let loaded = store
        .load_thread("auto-init-thread")
        .await
        .expect("load persisted thread")
        .expect("thread should exist");
    assert_eq!(loaded.id, "auto-init-thread");
    assert_eq!(loaded.message_count(), 1);
}

#[tokio::test]
async fn test_auto_initializes_schema_on_first_run_access() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store_without_ensure(&url).await;

    let mut run = RunRecord::new(
        "run-auto-init",
        "thread-auto-init",
        "",
        RunOrigin::AgUi,
        RunStatus::Running,
        100,
    );
    run.updated_at = 120;

    store
        .upsert_run(&run)
        .await
        .expect("run writes should auto-create tables");

    let loaded = tirea_contract::storage::RunReader::load_run(&store, "run-auto-init")
        .await
        .expect("load run")
        .expect("run should exist");
    assert_eq!(loaded.thread_id, "thread-auto-init");

    let current = store
        .load_current_run("thread-auto-init")
        .await
        .expect("load current run")
        .expect("current run should exist");
    assert_eq!(current.run_id, "run-auto-init");
}

#[tokio::test]
async fn test_save_replaces_messages_and_advances_version() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let initial = Thread::new("replace-thread").with_messages(vec![
        Message::user("hello-1").with_id("m1".to_string()),
        Message::assistant("hello-2").with_id("m2".to_string()),
    ]);
    store.save(&initial).await.unwrap();

    let replacement = Thread::new("replace-thread")
        .with_message(Message::user("hello-1-updated").with_id("m1".to_string()));
    store.save(&replacement).await.unwrap();

    let head = store.load("replace-thread").await.unwrap().unwrap();
    assert_eq!(head.version, 1, "save should increment thread version");
    assert_eq!(
        head.thread.messages.len(),
        1,
        "save should replace persisted messages"
    );
    assert_eq!(head.thread.messages[0].content, "hello-1-updated");
    assert_eq!(head.thread.messages[0].id.as_deref(), Some("m1"));
}

#[tokio::test]
async fn test_ensure_table_creates_thread_filter_indexes() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let _store = make_store(&url).await;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("failed to connect to Postgres");
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes WHERE schemaname = current_schema() AND tablename = 'agent_sessions'",
    )
    .fetch_all(&pool)
    .await
    .expect("load indexes");
    assert!(
        indexes
            .iter()
            .any(|index| index == "idx_agent_sessions_resource_id"),
        "resource_id filter index should exist"
    );
    assert!(
        indexes
            .iter()
            .any(|index| index == "idx_agent_sessions_parent_thread_id"),
        "parent_thread_id filter index should exist"
    );
}

#[tokio::test]
async fn test_mailbox_roundtrip_and_cancellation() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;
    let entry = mailbox_entry("run-pg-mailbox", "thread-pg-mailbox");

    store.enqueue_mailbox_entry(&entry).await.unwrap();

    let claimed = store
        .claim_mailbox_entries(10, "worker-pg", 10, 5_000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].status, MailboxEntryStatus::Claimed);

    let cancelled = store
        .cancel_mailbox_entry_by_run_id("run-pg-mailbox", 20)
        .await
        .unwrap()
        .expect("claimed mailbox entry should still be cancellable");
    assert_eq!(cancelled.status, MailboxEntryStatus::Cancelled);

    let loaded = store
        .load_mailbox_entry_by_run_id("run-pg-mailbox")
        .await
        .unwrap()
        .expect("mailbox entry should still be queryable");
    assert_eq!(loaded.status, MailboxEntryStatus::Cancelled);
}

#[tokio::test]
async fn test_mailbox_claim_by_run_id_ignores_available_at_for_inline_dispatch() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let mut entry = mailbox_entry("run-pg-inline", "thread-pg-inline");
    entry.available_at = i64::MAX as u64;
    store.enqueue_mailbox_entry(&entry).await.unwrap();

    let claimed = store
        .claim_mailbox_entries(10, "worker-batch", 10, 5_000)
        .await
        .unwrap();
    assert!(claimed.is_empty());

    let targeted = store
        .claim_mailbox_entry_by_run_id("run-pg-inline", "worker-inline", 10, 5_000)
        .await
        .unwrap()
        .expect("inline claim should succeed");
    assert_eq!(targeted.status, MailboxEntryStatus::Claimed);
    assert_eq!(targeted.claimed_by.as_deref(), Some("worker-inline"));
}

#[tokio::test]
async fn test_mailbox_interrupt_bumps_generation_and_supersedes_pending_entries() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let old_a = mailbox_entry("run-pg-old-a", "thread-pg-interrupt");
    let old_b = mailbox_entry("run-pg-old-b", "thread-pg-interrupt");
    store.enqueue_mailbox_entry(&old_a).await.unwrap();
    store.enqueue_mailbox_entry(&old_b).await.unwrap();

    let interrupted = store
        .interrupt_mailbox_thread("thread-pg-interrupt", 50)
        .await
        .unwrap();
    assert_eq!(interrupted.thread_state.current_generation, 1);
    assert_eq!(interrupted.superseded_entries.len(), 2);

    let superseded = store
        .load_mailbox_entry_by_run_id("run-pg-old-a")
        .await
        .unwrap()
        .expect("superseded entry should exist");
    assert_eq!(superseded.status, MailboxEntryStatus::Superseded);

    let next_generation = store
        .ensure_mailbox_thread_state("thread-pg-interrupt", 60)
        .await
        .unwrap()
        .current_generation;
    let mut fresh = mailbox_entry("run-pg-fresh", "thread-pg-interrupt");
    fresh.generation = next_generation;
    store.enqueue_mailbox_entry(&fresh).await.unwrap();

    let fresh_loaded = store
        .load_mailbox_entry_by_run_id("run-pg-fresh")
        .await
        .unwrap()
        .expect("fresh entry should exist");
    assert_eq!(fresh_loaded.generation, 1);
    assert_eq!(fresh_loaded.status, MailboxEntryStatus::Queued);
}

#[tokio::test]
async fn test_run_projection_roundtrip_and_filters() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let mut root = RunRecord::new(
        "run-root",
        "thread-a",
        "",
        RunOrigin::AgUi,
        RunStatus::Running,
        100,
    );
    root.updated_at = 150;
    store.upsert_run(&root).await.expect("upsert root");

    let mut child = RunRecord::new(
        "run-child",
        "thread-b",
        "",
        RunOrigin::Subagent,
        RunStatus::Done,
        200,
    );
    child.parent_run_id = Some("run-root".to_string());
    child.parent_thread_id = Some("thread-a".to_string());
    store.upsert_run(&child).await.expect("upsert child");

    let loaded = tirea_contract::storage::RunReader::load_run(&store, "run-child")
        .await
        .expect("load child")
        .expect("child exists");
    assert_eq!(loaded.parent_run_id.as_deref(), Some("run-root"));
    assert_eq!(loaded.origin, RunOrigin::Subagent);

    let page = tirea_contract::storage::RunReader::list_runs(
        &store,
        &RunQuery {
            status: Some(RunStatus::Done),
            origin: Some(RunOrigin::Subagent),
            ..Default::default()
        },
    )
    .await
    .expect("list runs");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].run_id, "run-child");

    let page = tirea_contract::storage::RunReader::list_runs(
        &store,
        &RunQuery {
            created_at_from: Some(80),
            created_at_to: Some(180),
            updated_at_from: Some(120),
            updated_at_to: Some(180),
            ..Default::default()
        },
    )
    .await
    .expect("list runs by timestamp");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].run_id, "run-root");

    let resolved = store
        .resolve_thread_id("run-root")
        .await
        .expect("resolve thread");
    assert_eq!(resolved.as_deref(), Some("thread-a"));

    store.delete_run("run-root").await.expect("delete run");
    assert!(
        tirea_contract::storage::RunReader::load_run(&store, "run-root")
            .await
            .expect("load deleted")
            .is_none()
    );
}

// ========================================================================
// load_current_run tests
// ========================================================================

#[tokio::test]
async fn test_load_current_run_returns_latest_non_terminal() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    // Completed run (terminal).
    let mut done = RunRecord::new("run-old", "t1", "", RunOrigin::AgUi, RunStatus::Done, 100);
    done.updated_at = 150;
    store.upsert_run(&done).await.expect("upsert done");

    // Active run (non-terminal).
    let mut active = RunRecord::new(
        "run-active",
        "t1",
        "",
        RunOrigin::AgUi,
        RunStatus::Running,
        200,
    );
    active.updated_at = 250;
    store.upsert_run(&active).await.expect("upsert active");

    let current = store
        .load_current_run("t1")
        .await
        .expect("load current run");
    assert_eq!(
        current.as_ref().map(|r| r.run_id.as_str()),
        Some("run-active"),
        "should return the non-terminal run"
    );
}

#[tokio::test]
async fn test_load_current_run_returns_none_when_all_terminal() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let done = RunRecord::new("run-d", "t2", "", RunOrigin::AiSdk, RunStatus::Done, 100);
    store.upsert_run(&done).await.expect("upsert done");

    assert!(
        store.load_current_run("t2").await.unwrap().is_none(),
        "should return None when all runs are terminal"
    );
}

#[tokio::test]
async fn test_upsert_run_rejects_multiple_active_runs_for_same_thread() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    // Insert an older terminal run and a newer active run.
    // The unique constraint allows at most one non-terminal run per thread.
    let r1 = RunRecord::new("run-a", "t3", "", RunOrigin::AgUi, RunStatus::Done, 100);
    let r2 = RunRecord::new("run-b", "t3", "", RunOrigin::AgUi, RunStatus::Waiting, 200);
    store.upsert_run(&r1).await.unwrap();
    store.upsert_run(&r2).await.unwrap();

    let current = store.load_current_run("t3").await.unwrap().unwrap();
    assert_eq!(
        current.run_id, "run-b",
        "should return the active run, ignoring terminal ones"
    );
}

// ========================================================================
// Tool call message round-trip tests
// ========================================================================

#[tokio::test]
async fn test_tool_call_message_roundtrip_via_save() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

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
    assert_eq!(assistant_msg.role, tirea_contract::Role::Assistant);
    let calls = assistant_msg.tool_calls.as_ref().expect("tool_calls lost");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "search");
    assert_eq!(calls[0].arguments, json!({"query": "rust"}));

    // Tool response message with tool_call_id
    let tool_msg = &loaded.messages[2];
    assert_eq!(tool_msg.role, tirea_contract::Role::Tool);
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(tool_msg.content, r#"{"result": "Rust is a language"}"#);
}

#[tokio::test]
async fn test_tool_call_message_roundtrip_via_create() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let tool_call = ToolCall::new("call_c", "calc", json!({"expr": "2+2"}));
    let thread = Thread::new("tool-create")
        .with_message(Message::assistant_with_tool_calls(
            "calculating",
            vec![tool_call],
        ))
        .with_message(Message::tool("call_c", "4"));

    store.create(&thread).await.unwrap();
    let head = store.load("tool-create").await.unwrap().unwrap();

    let calls = head.thread.messages[0]
        .tool_calls
        .as_ref()
        .expect("tool_calls lost after create");
    assert_eq!(calls[0].id, "call_c");
    assert_eq!(calls[0].name, "calc");

    assert_eq!(
        head.thread.messages[1].tool_call_id.as_deref(),
        Some("call_c")
    );
}

#[tokio::test]
async fn test_tool_call_message_roundtrip_via_append() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;
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
        .append("tool-append", &delta, VersionPrecondition::Exact(0))
        .await
        .unwrap();

    let head = store.load("tool-append").await.unwrap().unwrap();
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

#[tokio::test]
async fn test_tool_call_message_roundtrip_via_load_messages() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;

    let calls = vec![
        ToolCall::new("call_a", "search", json!({"q": "hello"})),
        ToolCall::new("call_b", "fetch", json!({"url": "https://example.com"})),
    ];
    let thread = Thread::new("tool-paged")
        .with_message(Message::user("do things"))
        .with_message(Message::assistant_with_tool_calls("multi-tool", calls))
        .with_message(Message::tool("call_a", "search result"))
        .with_message(Message::tool("call_b", "fetch result"));

    store.save(&thread).await.unwrap();

    let page = store
        .load_messages(
            "tool-paged",
            &MessageQuery {
                visibility: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.messages.len(), 4);

    // Assistant with multiple tool_calls
    let assistant = &page.messages[1].message;
    let tool_calls = assistant
        .tool_calls
        .as_ref()
        .expect("tool_calls lost in load_messages");
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_a");
    assert_eq!(tool_calls[0].name, "search");
    assert_eq!(tool_calls[1].id, "call_b");
    assert_eq!(tool_calls[1].name, "fetch");

    // Tool responses
    assert_eq!(
        page.messages[2].message.tool_call_id.as_deref(),
        Some("call_a")
    );
    assert_eq!(
        page.messages[3].message.tool_call_id.as_deref(),
        Some("call_b")
    );
}

#[tokio::test]
async fn test_load_messages_returns_error_when_row_is_corrupted() {
    let Some((_container, url)) = start_postgres().await else {
        return;
    };
    let store = make_store(&url).await;
    store
        .save(&Thread::new("corrupt-thread").with_message(Message::user("hello")))
        .await
        .expect("seed thread");

    let pool = sqlx::PgPool::connect(&url).await.expect("connect raw pool");
    sqlx::query(
        "INSERT INTO agent_messages (session_id, message_id, run_id, step_index, data) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("corrupt-thread")
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(Option::<i32>::None)
    .bind(json!({ "role": "user" })) // missing required `content`
    .execute(&pool)
    .await
    .expect("insert corrupted row");

    let err = store
        .load_messages("corrupt-thread", &MessageQuery::default())
        .await
        .expect_err("corrupted row must fail");
    match err {
        ThreadStoreError::Serialization(message) => {
            assert!(
                message.contains("failed to deserialize message row"),
                "unexpected error message: {message}"
            );
        }
        other => panic!("expected serialization error, got: {other:?}"),
    }
}
