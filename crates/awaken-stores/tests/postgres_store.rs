//! Integration tests for PostgresStore.
//!
//! Requires Docker with PostgreSQL. Tests are marked `#[ignore]` since they
//! need an external service. Run with:
//! ```bash
//! cargo test --package awaken-stores --features postgres --test postgres_store -- --ignored
//! ```

#![cfg(feature = "postgres")]

use awaken_contract::contract::config_store::{
    ConfigChangeKind, ConfigChangeNotifier, ConfigStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunQuery, RunStore, ThreadRunStore, ThreadStore};
use awaken_contract::thread::Thread;
use awaken_stores::PostgresStore;
use serde_json::json;
use tokio::time::{Duration, timeout};

mod support;
use support::make_run;

/// Helper to create a store connected to a test database.
/// Set DATABASE_URL env var to a PostgreSQL connection string.
async fn make_store() -> Option<PostgresStore> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    let store = PostgresStore::new(pool);
    store.ensure_schema().await.ok()?;
    Some(store)
}

async fn make_prefixed_store(prefix: &str) -> Option<PostgresStore> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    let unique = format!(
        "{}_{}_{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
    );
    let store = PostgresStore::with_prefix(pool, &unique);
    store.ensure_schema().await.ok()?;
    Some(store)
}

// ========================================================================
// ThreadStore
// ========================================================================

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn save_load_thread() {
    let Some(store) = make_store().await else {
        return;
    };
    let thread = Thread::with_id("pg-t-1");
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread("pg-t-1").await.unwrap().unwrap();
    assert_eq!(loaded.id, "pg-t-1");
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn load_nonexistent_thread() {
    let Some(store) = make_store().await else {
        return;
    };
    let loaded = store.load_thread("pg-nonexistent").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn list_threads_paginated() {
    let Some(store) = make_store().await else {
        return;
    };
    for i in 0..5 {
        store
            .save_thread(&Thread::with_id(format!("pg-list-{i}")))
            .await
            .unwrap();
    }
    let page = store.list_threads(0, 100).await.unwrap();
    assert!(page.len() >= 5);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn overwrite_thread() {
    let Some(store) = make_store().await else {
        return;
    };
    let thread = Thread::with_id("pg-overwrite").with_title("v1");
    store.save_thread(&thread).await.unwrap();

    let updated = Thread::with_id("pg-overwrite").with_title("v2");
    store.save_thread(&updated).await.unwrap();

    let loaded = store.load_thread("pg-overwrite").await.unwrap().unwrap();
    assert_eq!(loaded.metadata.title.as_deref(), Some("v2"));
}

// ========================================================================
// RunStore
// ========================================================================

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn create_and_load_run() {
    let Some(store) = make_store().await else {
        return;
    };
    let run = make_run("pg-run-1", "pg-t-1", 100);
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "pg-run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.thread_id, "pg-t-1");
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn latest_run() {
    let Some(store) = make_store().await else {
        return;
    };
    store
        .create_run(&make_run("pg-r1", "pg-t-latest", 100))
        .await
        .unwrap();
    store
        .create_run(&make_run("pg-r2", "pg-t-latest", 200))
        .await
        .unwrap();

    let latest = RunStore::latest_run(&store, "pg-t-latest")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.run_id, "pg-r2");
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn list_runs_with_filter() {
    let Some(store) = make_store().await else {
        return;
    };
    store
        .create_run(&make_run("pg-rf1", "pg-t-filter", 100))
        .await
        .unwrap();
    store
        .create_run(&make_run("pg-rf2", "pg-t-filter2", 200))
        .await
        .unwrap();

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("pg-t-filter".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.total >= 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn run_with_tokens() {
    let Some(store) = make_store().await else {
        return;
    };
    let mut run = make_run("pg-rtok", "pg-t-tok", 100);
    run.input_tokens = 500;
    run.output_tokens = 200;
    run.steps = 3;
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "pg-rtok")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.input_tokens, 500);
    assert_eq!(loaded.output_tokens, 200);
    assert_eq!(loaded.steps, 3);
}

// ========================================================================
// ThreadRunStore
// ========================================================================

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn checkpoint_and_load() {
    let Some(store) = make_store().await else {
        return;
    };
    let run = make_run("pg-cp-run", "pg-cp-thread", 42);
    let messages = vec![Message::user("u1"), Message::assistant("a1")];

    store
        .checkpoint("pg-cp-thread", &messages, &run)
        .await
        .unwrap();

    let loaded = ThreadStore::load_messages(&store, "pg-cp-thread")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.len(), 2);

    let loaded_run = RunStore::load_run(&store, "pg-cp-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_run.thread_id, "pg-cp-thread");

    let thread = ThreadStore::load_thread(&store, "pg-cp-thread")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.id, "pg-cp-thread");
    assert!(thread.metadata.created_at.is_some());
    assert!(thread.metadata.updated_at.is_some());
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn checkpoint_overwrites() {
    let Some(store) = make_store().await else {
        return;
    };
    store
        .checkpoint(
            "pg-cp-ow",
            &[Message::user("old")],
            &make_run("pg-cp-ow-r1", "pg-cp-ow", 100),
        )
        .await
        .unwrap();

    store
        .checkpoint(
            "pg-cp-ow",
            &[Message::user("new")],
            &make_run("pg-cp-ow-r2", "pg-cp-ow", 200),
        )
        .await
        .unwrap();

    let msgs = ThreadStore::load_messages(&store, "pg-cp-ow")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "new");
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn auto_initializes_schema() {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => return,
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let store = PostgresStore::with_prefix(pool, "auto_init_test");

    // First access should auto-create tables
    let loaded = store.load_thread("nonexistent").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn config_store_round_trip() {
    let Some(store) = make_prefixed_store("cfg_round_trip").await else {
        return;
    };

    ConfigStore::put(
        &store,
        "providers",
        "openai",
        &json!({
            "id": "openai",
            "adapter": "openai",
            "api_key": "sk-test"
        }),
    )
    .await
    .unwrap();
    ConfigStore::put(
        &store,
        "providers",
        "anthropic",
        &json!({
            "id": "anthropic",
            "adapter": "anthropic"
        }),
    )
    .await
    .unwrap();

    let stored = ConfigStore::get(&store, "providers", "openai")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["adapter"], "openai");

    let listed = ConfigStore::list(&store, "providers", 0, 10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].0, "anthropic");
    assert_eq!(listed[0].1["id"], "anthropic");
    assert_eq!(listed[1].0, "openai");
    assert_eq!(listed[1].1["id"], "openai");

    let paged = ConfigStore::list(&store, "providers", 1, 1).await.unwrap();
    assert_eq!(paged.len(), 1);
    assert_eq!(paged[0].0, "openai");

    ConfigStore::delete(&store, "providers", "openai")
        .await
        .unwrap();
    ConfigStore::delete(&store, "providers", "anthropic")
        .await
        .unwrap();
    assert!(
        ConfigStore::get(&store, "providers", "openai")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn config_store_large_limit_is_clamped() {
    let Some(store) = make_prefixed_store("cfg_large_limit").await else {
        return;
    };

    ConfigStore::put(
        &store,
        "providers",
        "alpha",
        &json!({
            "id": "alpha",
            "adapter": "openai"
        }),
    )
    .await
    .unwrap();

    let listed = ConfigStore::list(&store, "providers", 0, usize::MAX)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "alpha");
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn config_store_emits_notify_events() {
    let Some(store) = make_prefixed_store("cfg_notify").await else {
        return;
    };
    let mut subscriber = store.subscribe().await.unwrap();

    ConfigStore::put(
        &store,
        "mcp-servers",
        "notify",
        &json!({
            "id": "notify",
            "transport": "stdio",
            "command": "notify-mcp"
        }),
    )
    .await
    .unwrap();

    let put_event = timeout(Duration::from_secs(2), subscriber.next())
        .await
        .expect("timed out waiting for put notification")
        .unwrap();
    assert_eq!(put_event.namespace, "mcp-servers");
    assert_eq!(put_event.id, "notify");
    assert_eq!(put_event.kind, ConfigChangeKind::Put);

    ConfigStore::delete(&store, "mcp-servers", "notify")
        .await
        .unwrap();

    let delete_event = timeout(Duration::from_secs(2), subscriber.next())
        .await
        .expect("timed out waiting for delete notification")
        .unwrap();
    assert_eq!(delete_event.namespace, "mcp-servers");
    assert_eq!(delete_event.id, "notify");
    assert_eq!(delete_event.kind, ConfigChangeKind::Delete);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn deleting_missing_config_does_not_emit_notify_event() {
    let Some(store) = make_prefixed_store("cfg_missing_delete").await else {
        return;
    };
    let mut subscriber = store.subscribe().await.unwrap();

    ConfigStore::delete(&store, "mcp-servers", "missing")
        .await
        .unwrap();

    let result = timeout(Duration::from_millis(250), subscriber.next()).await;
    assert!(
        result.is_err(),
        "deleting a missing config entry should not emit a notify event"
    );
}
