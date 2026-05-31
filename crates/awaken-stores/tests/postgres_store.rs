#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
//! Integration tests for PostgresStore.
//!
//! Requires Docker with PostgreSQL. Tests are marked `#[ignore]` since they
//! need an external service. Run with:
//! ```bash
//! cargo test --package awaken-stores --features postgres --test postgres_store -- --ignored
//! ```

#![cfg(feature = "postgres")]

use awaken_server_contract::contract::config_store::{
    ConfigChangeKind, ConfigChangeNotifier, ConfigStore,
};
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::RunRecord;
use awaken_server_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessageSeqRange, RunMessageInput, RunMessageOutput, RunQuery,
    RunRequestSnapshot, RunStore, StorageError, ThreadParentFilter, ThreadQuery, ThreadRunStore,
    ThreadStore,
};
use awaken_server_contract::thread::Thread;
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
    let unique = unique_prefix(prefix)?;
    let store = PostgresStore::with_prefix(pool, &unique);
    store.ensure_schema().await.ok()?;
    Some(store)
}

async fn make_prefixed_store_with_pool(
    prefix: &str,
) -> Option<(PostgresStore, String, sqlx::PgPool)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    let unique = unique_prefix(prefix)?;
    let store = PostgresStore::with_prefix(pool.clone(), &unique);
    store.ensure_schema().await.ok()?;
    Some((store, unique, pool))
}

fn unique_prefix(prefix: &str) -> Option<String> {
    let uuid_short = uuid::Uuid::now_v7().simple().to_string();
    Some(format!(
        "pgs_{}_{}",
        &uuid_short[12..28],
        &prefix[..prefix.len().min(8)]
    ))
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

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn list_threads_query_filters_lineage() {
    let Some(store) = make_prefixed_store("pg_lineage_filter").await else {
        return;
    };
    let match_id = "pg-lineage-match";
    store
        .save_thread(
            &Thread::with_id(match_id)
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .unwrap();
    store
        .save_thread(
            &Thread::with_id("pg-lineage-other")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Parent("parent-1".to_string()),
            id_prefix: None,
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec![match_id.to_string()]);
    assert_eq!(page.total, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn list_threads_query_filters_root_threads() {
    let Some(store) = make_prefixed_store("pg_root_filter").await else {
        return;
    };
    store
        .save_thread(&Thread::with_id("pg-root-match").with_resource_id("resource-a"))
        .await
        .unwrap();
    store
        .save_thread(
            &Thread::with_id("pg-root-child")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-root-other").with_resource_id("resource-b"))
        .await
        .unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Root,
            id_prefix: None,
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["pg-root-match"]);
    assert_eq!(page.total, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn checkpoint_rejects_missing_parent_thread() {
    let Some(store) = make_prefixed_store("pg_missing_parent").await else {
        return;
    };
    let run = RunRecord {
        request: Some(RunRequestSnapshot {
            parent_thread_id: Some("missing-parent".to_string()),
            ..Default::default()
        }),
        status: awaken_server_contract::contract::lifecycle::RunStatus::Created,
        ..make_run("pg-missing-parent-run", "pg-child-thread", 1)
    };

    let error = store
        .checkpoint("pg-child-thread", &[], &run)
        .await
        .expect_err("checkpoint should reject unknown parent thread");

    assert!(
        matches!(error, StorageError::Validation(message) if message == "parent thread not found: missing-parent")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn delete_thread_with_detach_clears_direct_child_parent() {
    let Some(store) = make_prefixed_store("pg_detach_child").await else {
        return;
    };
    store
        .save_thread(&Thread::with_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-child").with_parent_thread_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-grandchild").with_parent_thread_id("pg-child"))
        .await
        .unwrap();

    store
        .delete_thread_with_strategy("pg-root", ChildThreadDeleteStrategy::Detach)
        .await
        .unwrap();

    assert!(store.load_thread("pg-root").await.unwrap().is_none());
    assert_eq!(
        store
            .load_thread("pg-child")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        None
    );
    assert_eq!(
        store
            .load_thread("pg-grandchild")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        Some("pg-child".to_string())
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn delete_thread_with_detach_rolls_back_on_child_update_failure() {
    let Some((store, prefix, pool)) = make_prefixed_store_with_pool("pg_detach_atomic").await
    else {
        return;
    };
    store
        .save_thread(&Thread::with_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-child-a").with_parent_thread_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-child-b").with_parent_thread_id("pg-root"))
        .await
        .unwrap();

    let function_name = format!("{prefix}_detach_fail");
    let trigger_name = format!("{prefix}_detach_fail_trigger");
    let table = format!("{prefix}_threads");
    let function_sql = format!(
        r#"
        CREATE OR REPLACE FUNCTION {function_name}() RETURNS trigger AS $$
        BEGIN
            IF NEW.id = 'pg-child-b' AND NEW.parent_thread_id IS NULL THEN
                RAISE EXCEPTION 'forced child detach failure';
            END IF;
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#
    );
    sqlx::query(&function_sql).execute(&pool).await.unwrap();
    let trigger_sql = format!(
        r#"
        CREATE TRIGGER {trigger_name}
        BEFORE UPDATE ON {table}
        FOR EACH ROW
        EXECUTE FUNCTION {function_name}()
        "#
    );
    sqlx::query(&trigger_sql).execute(&pool).await.unwrap();

    let error = store
        .delete_thread_with_strategy("pg-root", ChildThreadDeleteStrategy::Detach)
        .await
        .expect_err("detach should roll back when a child update fails");

    assert!(matches!(
        error,
        StorageError::Io(message) if message.contains("forced child detach failure")
    ));
    assert!(store.load_thread("pg-root").await.unwrap().is_some());
    assert_eq!(
        store
            .load_thread("pg-child-a")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        Some("pg-root".to_string())
    );
    assert_eq!(
        store
            .load_thread("pg-child-b")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        Some("pg-root".to_string())
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn delete_thread_with_reject_preserves_tree() {
    let Some(store) = make_prefixed_store("pg_reject_child").await else {
        return;
    };
    store
        .save_thread(&Thread::with_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-child").with_parent_thread_id("pg-root"))
        .await
        .unwrap();

    let error = store
        .delete_thread_with_strategy("pg-root", ChildThreadDeleteStrategy::Reject)
        .await
        .expect_err("reject strategy should fail");

    assert!(
        matches!(error, StorageError::Validation(message) if message.contains("child threads"))
    );
    assert!(store.load_thread("pg-root").await.unwrap().is_some());
    assert!(store.load_thread("pg-child").await.unwrap().is_some());
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn delete_thread_with_cascade_removes_descendants() {
    let Some(store) = make_prefixed_store("pg_cascade_child").await else {
        return;
    };
    store
        .save_thread(&Thread::with_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-child").with_parent_thread_id("pg-root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("pg-grandchild").with_parent_thread_id("pg-child"))
        .await
        .unwrap();

    store
        .delete_thread_with_strategy("pg-root", ChildThreadDeleteStrategy::Cascade)
        .await
        .unwrap();

    assert!(store.load_thread("pg-root").await.unwrap().is_none());
    assert!(store.load_thread("pg-child").await.unwrap().is_none());
    assert!(store.load_thread("pg-grandchild").await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn ensure_schema_normalizes_legacy_thread_lineage_columns_from_json() {
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    let prefix = format!(
        "pg_backfill_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let store = PostgresStore::with_prefix(pool.clone(), &prefix);
    store.ensure_schema().await.unwrap();

    let table = format!("{prefix}_threads");
    let legacy_match = {
        let mut thread = Thread::with_id("legacy-match");
        thread.resource_id = Some(" resource-a ".to_string());
        thread.parent_thread_id = Some(" parent-1 ".to_string());
        thread
    };
    let legacy_blank_parent = {
        let mut thread = Thread::with_id("legacy-blank-parent");
        thread.resource_id = Some(" resource-a ".to_string());
        thread.parent_thread_id = Some("   ".to_string());
        thread
    };
    let insert_sql = format!("INSERT INTO {} (id, data) VALUES ($1, $2)", table);
    for legacy in [&legacy_match, &legacy_blank_parent] {
        let data = serde_json::to_value(legacy).unwrap();
        sqlx::query(&insert_sql)
            .bind(&legacy.id)
            .bind(&data)
            .execute(&pool)
            .await
            .unwrap();
    }
    let clear_sql = format!(
        "UPDATE {} SET resource_id = NULL, parent_thread_id = NULL WHERE id = $1",
        table
    );
    for legacy_id in [&legacy_match.id, &legacy_blank_parent.id] {
        sqlx::query(&clear_sql)
            .bind(legacy_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    let migrated = PostgresStore::with_prefix(pool.clone(), &prefix);
    migrated.ensure_schema().await.unwrap();

    let loaded_match = migrated
        .load_thread(&legacy_match.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_match.resource_id.as_deref(), Some("resource-a"));
    assert_eq!(loaded_match.parent_thread_id.as_deref(), Some("parent-1"));

    let loaded_blank_parent = migrated
        .load_thread(&legacy_blank_parent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded_blank_parent.resource_id.as_deref(),
        Some("resource-a")
    );
    assert_eq!(loaded_blank_parent.parent_thread_id, None);

    let raw_sql = format!(
        "SELECT resource_id, parent_thread_id FROM {} WHERE id = $1",
        table
    );
    let match_columns: (Option<String>, Option<String>) = sqlx::query_as(&raw_sql)
        .bind(&legacy_match.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(match_columns.0.as_deref(), Some("resource-a"));
    assert_eq!(match_columns.1.as_deref(), Some("parent-1"));

    let blank_columns: (Option<String>, Option<String>) = sqlx::query_as(&raw_sql)
        .bind(&legacy_blank_parent.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(blank_columns.0.as_deref(), Some("resource-a"));
    assert_eq!(blank_columns.1, None);

    let page = migrated
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Parent("parent-1".to_string()),
            id_prefix: None,
        })
        .await
        .unwrap();
    assert_eq!(page.items, vec![legacy_match.id.clone()]);
}

// ========================================================================
// RunStore
// ========================================================================

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn create_and_load_run() {
    let Some(store) = make_prefixed_store("run_create").await else {
        return;
    };
    let mut run = make_run("pg-run-1", "pg-t-1", 100);
    run.input = Some(RunMessageInput {
        thread_id: "pg-t-1".to_string(),
        range: MessageSeqRange::new(1, 2),
        trigger_message_ids: vec!["pg-m-1".to_string()],
        selected_message_ids: Vec::new(),
        context_policy: None,
        compacted_snapshot_id: None,
    });
    run.output = Some(RunMessageOutput {
        thread_id: "pg-t-1".to_string(),
        range: MessageSeqRange::new(3, 3),
        message_ids: vec!["pg-m-3".to_string()],
    });
    store.create_run(&run).await.unwrap();

    let loaded = RunStore::load_run(&store, "pg-run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.thread_id, "pg-t-1");
    assert_eq!(loaded.input.unwrap().range.unwrap().to_seq, 2);
    assert_eq!(
        loaded.output.unwrap().message_ids,
        vec!["pg-m-3".to_string()]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn latest_run() {
    let Some(store) = make_prefixed_store("run_latest").await else {
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
    let Some(store) = make_prefixed_store("run_filter").await else {
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
    let Some(store) = make_prefixed_store("run_tokens").await else {
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

#[tokio::test]
#[ignore]
async fn list_runs_filters_by_id_prefix() {
    let Some(store) = make_prefixed_store("pg_runs_prefix").await else {
        return;
    };
    store
        .checkpoint("sa-t1", &[], &make_run("sa-r1", "sa-t1", 1))
        .await
        .unwrap();
    store
        .checkpoint("sa-t2", &[], &make_run("sa-r2", "sa-t2", 2))
        .await
        .unwrap();
    store
        .checkpoint("sb-t1", &[], &make_run("sb-r1", "sb-t1", 3))
        .await
        .unwrap();

    let page = store
        .list_runs(&RunQuery {
            offset: 0,
            limit: 50,
            thread_id: None,
            status: None,
            id_prefix: Some("sa-".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(page.total, 2);
    assert!(page.items.iter().all(|r| r.thread_id.starts_with("sa-")));
}

#[tokio::test]
#[ignore]
async fn list_threads_query_filters_by_id_prefix() {
    let Some(store) = make_prefixed_store("pg_threads_prefix").await else {
        return;
    };
    store.save_thread(&Thread::with_id("sa-t1")).await.unwrap();
    store.save_thread(&Thread::with_id("sa-t2")).await.unwrap();
    store.save_thread(&Thread::with_id("sb-t1")).await.unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 50,
            resource_id: None,
            parent_filter: ThreadParentFilter::Any,
            id_prefix: Some("sa-".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(page.total, 2);
    assert!(page.items.iter().all(|id| id.starts_with("sa-")));
}

#[tokio::test]
#[ignore]
async fn list_threads_query_id_prefix_paginates_and_binds_cursor() {
    let Some(store) = make_prefixed_store("pg_threads_prefix_escape").await else {
        return;
    };
    let prefix = "scope:5:a%_\\:";
    store
        .save_thread(&Thread::with_id(format!("{prefix}thread-1")))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id(format!("{prefix}thread-2")))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("scope:6:a%_\\:thread-3"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("scope:5:aXY\\:thread-4"))
        .await
        .unwrap();

    let query = ThreadQuery {
        offset: 0,
        limit: 1,
        resource_id: None,
        parent_filter: ThreadParentFilter::Any,
        id_prefix: Some(prefix.to_string()),
    };
    let page = store.list_threads_query(&query).await.unwrap();

    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 1);
    assert!(page.items.iter().all(|id| id.starts_with(prefix)));
    assert!(page.has_more);
    let cursor = page.next_cursor.expect("prefix page should expose cursor");
    let next_offset = query.decode_cursor(&cursor).unwrap();
    assert_eq!(next_offset, 1);
    assert!(
        ThreadQuery {
            id_prefix: Some("scope:6:a%_\\:".to_string()),
            ..query.clone()
        }
        .decode_cursor(&cursor)
        .is_err()
    );
    assert!(
        ThreadQuery {
            id_prefix: None,
            ..query.clone()
        }
        .decode_cursor(&cursor)
        .is_err()
    );

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: next_offset,
            ..query
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 1);
    assert!(page.items.iter().all(|id| id.starts_with(prefix)));
    assert!(!page.has_more);
}
