#![cfg(feature = "postgres")]

use std::collections::BTreeMap;

use awaken_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventCursor, EventLookup, EventReader,
    EventScope, EventStoreError, EventSubscriber, EventWriter, SubscribeStart,
};
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxStatus, OutboxStore,
};
use awaken_stores::PostgresStore;
use futures::StreamExt;
use serde_json::Value;
use sqlx::{PgPool, Row};

async fn test_pool() -> PgPool {
    let url = std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://localhost/awaken_test".to_string());
    PgPool::connect(&url).await.unwrap()
}

async fn test_store(prefix: &str) -> PostgresStore {
    PostgresStore::with_prefix(test_pool().await, prefix)
}

fn unique_prefix(name: &str) -> String {
    format!("test_event_{}_{}", name, uuid::Uuid::now_v7().simple())
}

fn kind(name: &str) -> CanonicalEventKind {
    CanonicalEventKind::new(name).unwrap()
}

fn draft(thread_id: &str, run_id: &str, value: i64) -> CanonicalEventDraft {
    CanonicalEventDraft::new(
        vec![EventScope::thread(thread_id), EventScope::run(run_id)],
        kind("RunStarted"),
        serde_json::json!({ "value": value }),
        "test",
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_schema_initializes_idempotently() {
    let prefix = unique_prefix("schema");
    let store = test_store(&prefix).await;
    store.ensure_schema().await.unwrap();
    store.ensure_schema().await.unwrap();

    let pool = test_pool().await;
    let count: i64 = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS count
         FROM information_schema.tables
         WHERE table_schema = current_schema()
           AND table_name = ANY($1)",
    )
    .bind(vec![
        format!("{prefix}_events"),
        format!("{prefix}_event_scope_index"),
        format!("{prefix}_event_scope_counters"),
        format!("{prefix}_event_idempotency"),
    ])
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("count")
    .unwrap();

    assert_eq!(count, 4);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_store_append_list_and_count() {
    let store = test_store(&unique_prefix("append")).await;
    let scope = EventScope::thread("t1");

    store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();

    let page = store.list(scope.clone(), None, 10).await.unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].payload["value"], Value::from(1));
    assert_eq!(page.events[1].payload["value"], Value::from(2));
    assert_eq!(store.count(scope).await.unwrap(), 2);
    let outbox = store
        .list_outbox(Some(OutboxStatus::Pending), 10)
        .await
        .unwrap();
    assert_eq!(outbox.len(), 2);
    assert_eq!(outbox[0].lane, OUTBOX_LANE_CANONICAL);
    assert_eq!(outbox[0].target, OUTBOX_TARGET_PROTOCOL_PROJECTOR);
    assert_eq!(outbox[0].payload["event_kind"], "RunStarted");
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_store_loads_event_by_id() {
    let store = test_store(&unique_prefix("load")).await;
    let appended = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();

    let loaded = store.load_event(appended.event_id()).await.unwrap();

    assert_eq!(loaded, appended.event);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_store_multi_scope_and_cursor_replay() {
    let store = test_store(&unique_prefix("scope")).await;
    let thread_scope = EventScope::thread("t1");
    let run_scope = EventScope::run("r1");

    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();

    assert_eq!(
        store.list(run_scope, None, 10).await.unwrap().events.len(),
        1
    );
    let cursor = first
        .cursors_by_scope()
        .get(&thread_scope)
        .cloned()
        .unwrap();
    let page = store.list(thread_scope, Some(cursor), 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].payload["value"], Value::from(2));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_store_idempotency_and_conflict() {
    let store = test_store(&unique_prefix("idem")).await;
    let options = AppendOptions {
        writer_id: Some("writer".into()),
        idempotency_key: Some("key".into()),
        expected_prior_cursors: BTreeMap::new(),
    };

    let first = store
        .append(draft("t1", "r1", 1), options.clone())
        .await
        .unwrap();
    let second = store
        .append(draft("t1", "r1", 1), options.clone())
        .await
        .unwrap();
    assert_eq!(first.event_id(), second.event_id());

    let err = store
        .append(draft("t1", "r1", 2), options)
        .await
        .unwrap_err();
    assert!(matches!(err, EventStoreError::IdempotencyConflict(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_store_expected_cursor_conflict() {
    let store = test_store(&unique_prefix("cursor")).await;
    store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    let mut expected_prior_cursors = BTreeMap::new();
    expected_prior_cursors.insert(EventScope::thread("t1"), EventCursor::new("wrong").unwrap());

    let err = store
        .append(
            draft("t1", "r2", 2),
            AppendOptions {
                writer_id: None,
                idempotency_key: None,
                expected_prior_cursors,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, EventStoreError::ExpectedCursorConflict(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_subscribe_from_cursor_replays_then_tails() {
    let store = test_store(&unique_prefix("subscribe_cursor")).await;
    let scope = EventScope::thread("t1");
    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();
    let cursor = first.cursors_by_scope().get(&scope).cloned().unwrap();

    let mut handle = store
        .subscribe(scope, SubscribeStart::FromCursor(cursor.clone()))
        .await
        .unwrap();
    assert_eq!(handle.start_cursor, Some(cursor));
    let replayed = handle.stream.next().await.unwrap().unwrap();
    assert_eq!(replayed.payload["value"], Value::from(2));

    store
        .append(draft("t1", "r3", 3), AppendOptions::default())
        .await
        .unwrap();
    let tailed = tokio::time::timeout(std::time::Duration::from_secs(2), handle.stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(tailed.payload["value"], Value::from(3));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_event_subscribe_from_now_starts_after_high_water() {
    let store = test_store(&unique_prefix("subscribe_now")).await;
    let scope = EventScope::thread("t1");
    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();

    let mut handle = store
        .subscribe(scope.clone(), SubscribeStart::FromNow)
        .await
        .unwrap();
    assert_eq!(
        handle.start_cursor,
        first.cursors_by_scope().get(&scope).cloned()
    );
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();

    let tailed = tokio::time::timeout(std::time::Duration::from_secs(2), handle.stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(tailed.payload["value"], Value::from(2));
}
