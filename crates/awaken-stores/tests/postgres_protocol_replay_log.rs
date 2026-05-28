#![cfg(feature = "postgres")]

use awaken_server_contract::contract::event_store::{CanonicalEventId, EventCursor, EventScope};
use awaken_server_contract::contract::outbox::{
    OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_PROTOCOL_FANOUT, OutboxStatus, OutboxStore,
};
use awaken_server_contract::contract::protocol_replay_log::{
    ProtocolReplayCursor, ProtocolReplayDraft, ProtocolReplayError, ProtocolReplayId,
    ProtocolReplayLookup, ProtocolReplayReader, ProtocolReplayRedactionState, ProtocolReplayWriter,
    ProtocolStreamKey, SourceEventCursor,
};
use awaken_stores::PostgresStore;
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
    let uuid_short = uuid::Uuid::now_v7().simple().to_string();
    format!("pgr_{}_{}", &uuid_short[..12], &name[..name.len().min(8)])
}

fn stream(protocol_version: &str) -> ProtocolStreamKey {
    ProtocolStreamKey::new("thread:t1", "ai-sdk", protocol_version).unwrap()
}

fn draft(wire_event_id: &str, protocol_version: &str, payload: &[u8]) -> ProtocolReplayDraft {
    draft_for_stream("thread:t1", wire_event_id, protocol_version, payload)
}

fn draft_for_stream(
    stream_id: &str,
    wire_event_id: &str,
    protocol_version: &str,
    payload: &[u8],
) -> ProtocolReplayDraft {
    ProtocolReplayDraft::new(
        stream_id,
        "ai-sdk",
        protocol_version,
        "projector-1",
        wire_event_id,
        "agent.message",
        payload.to_vec(),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_schema_initializes_idempotently() {
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
        format!("{prefix}_protocol_replay_log"),
        format!("{prefix}_protocol_replay_counters"),
    ])
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("count")
    .unwrap();

    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_append_list_and_paginate() {
    let store = test_store(&unique_prefix("append")).await;
    let first = store
        .append_replay(draft("wire-1", "v6", br#"{"n":1}"#))
        .await
        .unwrap();
    store
        .append_replay(draft("wire-2", "v6", br#"{"n":2}"#))
        .await
        .unwrap();

    let page = store.list_replay(stream("v6"), None, 1).await.unwrap();
    assert_eq!(page.records.len(), 1);
    assert!(page.has_more);
    assert_eq!(page.next_cursor, Some(first.record.protocol_replay_cursor));

    let page = store
        .list_replay(stream("v6"), page.next_cursor, 10)
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].wire_payload_bytes, br#"{"n":2}"#);
    let outbox = store
        .list_outbox(Some(OutboxStatus::Pending), 10)
        .await
        .unwrap();
    assert_eq!(outbox.len(), 2);
    assert_eq!(outbox[0].lane, OUTBOX_LANE_PROTOCOL_REPLAY);
    assert_eq!(outbox[0].target, OUTBOX_TARGET_PROTOCOL_FANOUT);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_protocol_versions_are_isolated() {
    let store = test_store(&unique_prefix("versions")).await;
    store
        .append_replay(draft("wire-1", "v6", b"v6"))
        .await
        .unwrap();
    store
        .append_replay(draft("wire-1", "v7", b"v7"))
        .await
        .unwrap();

    let v6 = store.list_replay(stream("v6"), None, 10).await.unwrap();
    let v7 = store.list_replay(stream("v7"), None, 10).await.unwrap();

    assert_eq!(v6.records[0].wire_payload_bytes, b"v6");
    assert_eq!(v7.records[0].wire_payload_bytes, b"v7");
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_retry_and_conflict() {
    let store = test_store(&unique_prefix("retry")).await;
    let first = store
        .append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();
    let second = store
        .append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();
    assert_eq!(
        first.record.protocol_replay_id,
        second.record.protocol_replay_id
    );

    let err = store
        .append_replay(draft("wire-1", "v6", b"different"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProtocolReplayError::Conflict(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_loads_row_by_identity() {
    let store = test_store(&unique_prefix("lookup")).await;
    let written = store
        .append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap()
        .record;

    let loaded = store
        .load_replay(&written.protocol_replay_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.protocol_replay_id, written.protocol_replay_id);
    assert_eq!(loaded.wire_payload_bytes, b"one");
    assert!(
        store
            .load_replay(&ProtocolReplayId::new("missing").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_wire_event_ids_are_global_per_protocol_version() {
    let store = test_store(&unique_prefix("wire_global")).await;
    store
        .append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();

    let err = store
        .append_replay(draft_for_stream("thread:t2", "wire-1", "v6", b"same"))
        .await
        .unwrap_err();

    assert!(matches!(err, ProtocolReplayError::Conflict(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_cursor_and_source_metadata() {
    let store = test_store(&unique_prefix("cursor")).await;
    let first = store
        .append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap();
    let mut replay = draft("wire-2", "v6", b"two");
    let source_id = CanonicalEventId::new("evt_1").unwrap();
    replay.source_event_ids.push(source_id.clone());
    replay.source_event_cursors.push(SourceEventCursor::new(
        source_id,
        EventScope::thread("t1"),
        EventCursor::new("cur_1").unwrap(),
    ));
    replay.redaction_state = ProtocolReplayRedactionState::Redacted;
    store.append_replay(replay).await.unwrap();

    let page = store
        .list_replay(stream("v6"), Some(first.record.protocol_replay_cursor), 10)
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].source_event_ids.len(), 1);
    assert_eq!(page.records[0].source_event_cursors.len(), 1);
    assert_eq!(
        page.records[0].redaction_state,
        ProtocolReplayRedactionState::Redacted
    );

    let err = store
        .list_replay(
            stream("v6"),
            Some(ProtocolReplayCursor::new("missing").unwrap()),
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProtocolReplayError::Integrity(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_expired_rows_are_omitted() {
    let store = test_store(&unique_prefix("expiry")).await;
    let mut expired = draft("wire-expired", "v6", b"expired");
    expired.expires_at = Some(1);
    let expired = store.append_replay(expired).await.unwrap();
    store
        .append_replay(draft("wire-live", "v6", b"live"))
        .await
        .unwrap();

    let page = store.list_replay(stream("v6"), None, 10).await.unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].wire_payload_bytes, b"live");

    let err = store
        .list_replay(
            stream("v6"),
            Some(expired.record.protocol_replay_cursor),
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProtocolReplayError::CursorExpired(_)));
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_protocol_replay_missing_cursor_row_inside_stream_is_integrity_error() {
    let prefix = unique_prefix("missing_cursor");
    let store = test_store(&prefix).await;
    let first = store
        .append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap()
        .record;
    store
        .append_replay(draft("wire-2", "v6", b"two"))
        .await
        .unwrap();

    let pool = test_pool().await;
    let table = format!("{prefix}_protocol_replay_log");
    let sql = format!("DELETE FROM {table} WHERE protocol_replay_id = $1");
    sqlx::query(&sql)
        .bind(first.protocol_replay_id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let err = store
        .list_replay(stream("v6"), Some(first.protocol_replay_cursor), 10)
        .await
        .unwrap_err();

    assert!(matches!(err, ProtocolReplayError::Integrity(_)));
}
