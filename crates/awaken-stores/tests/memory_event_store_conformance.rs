use awaken_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventCursor, EventLookup, EventReader,
    EventScope, EventStoreError, EventSubscriber, EventWriter, SubscribeStart,
};
use awaken_stores::InMemoryEventStore;
use futures::StreamExt;
use serde_json::Value;

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
async fn append_then_list_preserves_scope_order() {
    let store = InMemoryEventStore::new();
    let scope = EventScope::thread("t1");

    store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();

    let page = store.list(scope, None, 10).await.unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].payload["value"], Value::from(1));
    assert_eq!(page.events[1].payload["value"], Value::from(2));
    assert!(!page.has_more);
    assert_eq!(store.count(EventScope::thread("t1")).await.unwrap(), 2);
}

#[tokio::test]
async fn multi_scope_append_is_queryable_by_each_scope() {
    let store = InMemoryEventStore::new();
    let appended = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();

    let by_thread = store
        .list(EventScope::thread("t1"), None, 10)
        .await
        .unwrap();
    let by_run = store.list(EventScope::run("r1"), None, 10).await.unwrap();

    assert_eq!(by_thread.events.len(), 1);
    assert_eq!(by_run.events.len(), 1);
    assert_eq!(by_thread.events[0].event_id, *appended.event_id());
    assert_eq!(by_run.events[0].event_id, *appended.event_id());
}

#[tokio::test]
async fn load_event_returns_event_by_id() {
    let store = InMemoryEventStore::new();
    let appended = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();

    let loaded = store.load_event(appended.event_id()).await.unwrap();

    assert_eq!(loaded, appended.event);
}

#[tokio::test]
async fn list_from_cursor_returns_events_after_cursor() {
    let store = InMemoryEventStore::new();
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

    let page = store.list(scope, Some(cursor), 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].payload["value"], Value::from(2));
}

#[tokio::test]
async fn list_returns_cursor_expired_for_unknown_cursor() {
    let store = InMemoryEventStore::new();
    store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();

    let err = store
        .list(
            EventScope::thread("t1"),
            Some(EventCursor::new("unknown").unwrap()),
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EventStoreError::CursorExpired(_)));
}

#[tokio::test]
async fn idempotency_retry_returns_original_event() {
    let store = InMemoryEventStore::new();
    let options = AppendOptions {
        writer_id: Some("writer".into()),
        idempotency_key: Some("key".into()),
        expected_prior_cursors: Default::default(),
    };

    let first = store
        .append(draft("t1", "r1", 1), options.clone())
        .await
        .unwrap();
    let second = store.append(draft("t1", "r1", 1), options).await.unwrap();

    assert_eq!(first.event_id(), second.event_id());
    assert_eq!(store.count(EventScope::thread("t1")).await.unwrap(), 1);
}

#[tokio::test]
async fn subscribe_from_now_never_duplicates_concurrent_appends() {
    use futures::StreamExt;
    use tokio::time::{Duration, timeout};

    let store = InMemoryEventStore::new();
    let scope = EventScope::thread("t1");
    // Seed an event so the FromNow high-water cursor is non-trivial.
    store
        .append(draft("t1", "r1", 0), AppendOptions::default())
        .await
        .unwrap();

    let mut subscribers = Vec::new();
    for _ in 0..16 {
        let store = store.clone();
        let scope = scope.clone();
        subscribers.push(tokio::spawn(async move {
            let handle = store
                .subscribe(scope, SubscribeStart::FromNow)
                .await
                .unwrap();
            let mut stream = handle.stream;
            let mut event_ids = Vec::new();
            // Drain for a short window after appends complete.
            while let Ok(Some(Ok(event))) = timeout(Duration::from_millis(150), stream.next()).await
            {
                event_ids.push(event.event_id.as_str().to_string());
            }
            event_ids
        }));
    }

    // Race appends against late-arriving subscribers.
    tokio::time::sleep(Duration::from_millis(5)).await;
    for value in 1..=32 {
        store
            .append(draft("t1", "r1", value), AppendOptions::default())
            .await
            .unwrap();
    }

    for task in subscribers {
        let seen = task.await.unwrap();
        let mut deduped = seen.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            seen.len(),
            deduped.len(),
            "FromNow subscriber received duplicate event ids: {seen:?}"
        );
    }
}

#[tokio::test]
async fn idempotency_retry_with_different_origin_or_schema_version_returns_original() {
    let store = InMemoryEventStore::new();
    let options = AppendOptions {
        writer_id: Some("writer".into()),
        idempotency_key: Some("key".into()),
        expected_prior_cursors: Default::default(),
    };

    let first = store
        .append(draft("t1", "r1", 1), options.clone())
        .await
        .unwrap();

    // ADR-0034 D5: origin and schema_version are NOT part of the
    // idempotency basis; retries differing only in those fields must
    // return the original event, not IdempotencyConflict.
    let mut retry = draft("t1", "r1", 1);
    retry.origin = "ai-sdk".to_string();
    retry.schema_version = 17;

    let second = store.append(retry, options).await.unwrap();
    assert_eq!(first.event_id(), second.event_id());
    assert_eq!(store.count(EventScope::thread("t1")).await.unwrap(), 1);
}

#[tokio::test]
async fn idempotency_reuse_with_different_input_conflicts() {
    let store = InMemoryEventStore::new();
    let options = AppendOptions {
        writer_id: Some("writer".into()),
        idempotency_key: Some("key".into()),
        expected_prior_cursors: Default::default(),
    };

    store
        .append(draft("t1", "r1", 1), options.clone())
        .await
        .unwrap();
    let err = store
        .append(draft("t1", "r1", 2), options)
        .await
        .unwrap_err();

    assert!(matches!(err, EventStoreError::IdempotencyConflict(_)));
}

#[tokio::test]
async fn expected_prior_cursor_mismatch_conflicts() {
    let store = InMemoryEventStore::new();
    store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    let mut expected_prior_cursors = std::collections::BTreeMap::new();
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
async fn expected_prior_cursor_match_allows_append() {
    let store = InMemoryEventStore::new();
    let scope = EventScope::thread("t1");
    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    let mut expected_prior_cursors = std::collections::BTreeMap::new();
    expected_prior_cursors.insert(
        scope.clone(),
        first.cursors_by_scope().get(&scope).cloned().unwrap(),
    );

    store
        .append(
            draft("t1", "r2", 2),
            AppendOptions {
                writer_id: None,
                idempotency_key: None,
                expected_prior_cursors,
            },
        )
        .await
        .unwrap();

    assert_eq!(store.count(scope).await.unwrap(), 2);
}

#[tokio::test]
async fn subscribe_from_cursor_replays_history_then_live_tail_without_gap() {
    let store = InMemoryEventStore::new();
    let scope = EventScope::thread("t1");
    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    let first_cursor = first.cursors_by_scope().get(&scope).cloned().unwrap();
    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();

    let mut handle = store
        .subscribe(scope, SubscribeStart::FromCursor(first_cursor))
        .await
        .unwrap();

    let replayed = handle.stream.next().await.unwrap().unwrap();
    assert_eq!(replayed.payload["value"], Value::from(2));

    store
        .append(draft("t1", "r3", 3), AppendOptions::default())
        .await
        .unwrap();
    let live = handle.stream.next().await.unwrap().unwrap();
    assert_eq!(live.payload["value"], Value::from(3));
}

#[tokio::test]
async fn subscribe_from_now_returns_high_water_cursor_and_only_live_events() {
    let store = InMemoryEventStore::new();
    let scope = EventScope::thread("t1");
    let first = store
        .append(draft("t1", "r1", 1), AppendOptions::default())
        .await
        .unwrap();
    let first_cursor = first.cursors_by_scope().get(&scope).cloned().unwrap();

    let mut handle = store
        .subscribe(scope, SubscribeStart::FromNow)
        .await
        .unwrap();
    assert_eq!(handle.start_cursor, Some(first_cursor));

    store
        .append(draft("t1", "r2", 2), AppendOptions::default())
        .await
        .unwrap();
    let live = handle.stream.next().await.unwrap().unwrap();
    assert_eq!(live.payload["value"], Value::from(2));
}
