use awaken_server_contract::contract::event_store::{CanonicalEventId, EventCursor, EventScope};
use awaken_server_contract::contract::protocol_replay_log::{
    ProtocolReplayCursor, ProtocolReplayDraft, ProtocolReplayError, ProtocolReplayId,
    ProtocolReplayLookup, ProtocolReplayReader, ProtocolReplayRedactionState, ProtocolReplayWriter,
    ProtocolStreamKey, SourceEventCursor,
};
use awaken_stores::InMemoryProtocolReplayLog;

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
async fn append_then_list_preserves_byte_payload_order() {
    let log = InMemoryProtocolReplayLog::new();
    log.append_replay(draft("wire-1", "v6", br#"{"n":1}"#))
        .await
        .unwrap();
    log.append_replay(draft("wire-2", "v6", br#"{"n":2}"#))
        .await
        .unwrap();

    let page = log.list_replay(stream("v6"), None, 10).await.unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.records[0].wire_payload_bytes, br#"{"n":1}"#);
    assert_eq!(page.records[1].wire_payload_bytes, br#"{"n":2}"#);
    assert!(!page.has_more);
}

#[tokio::test]
async fn protocol_version_streams_are_isolated() {
    let log = InMemoryProtocolReplayLog::new();
    log.append_replay(draft("wire-1", "v6", b"v6"))
        .await
        .unwrap();
    log.append_replay(draft("wire-1", "v7", b"v7"))
        .await
        .unwrap();

    let v6 = log.list_replay(stream("v6"), None, 10).await.unwrap();
    let v7 = log.list_replay(stream("v7"), None, 10).await.unwrap();

    assert_eq!(v6.records.len(), 1);
    assert_eq!(v7.records.len(), 1);
    assert_eq!(v6.records[0].wire_payload_bytes, b"v6");
    assert_eq!(v7.records[0].wire_payload_bytes, b"v7");
}

#[tokio::test]
async fn wire_event_retry_returns_original_row() {
    let log = InMemoryProtocolReplayLog::new();
    let first = log
        .append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();
    let second = log
        .append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();

    assert_eq!(
        first.record.protocol_replay_id,
        second.record.protocol_replay_id
    );
    assert_eq!(
        first.record.protocol_replay_cursor,
        second.record.protocol_replay_cursor
    );
    assert_eq!(
        log.list_replay(stream("v6"), None, 10)
            .await
            .unwrap()
            .records
            .len(),
        1
    );
}

#[tokio::test]
async fn load_replay_returns_row_by_identity() {
    let log = InMemoryProtocolReplayLog::new();
    let written = log
        .append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap()
        .record;

    let loaded = log
        .load_replay(&written.protocol_replay_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.protocol_replay_id, written.protocol_replay_id);
    assert_eq!(loaded.wire_payload_bytes, b"one");
    assert!(
        log.load_replay(&ProtocolReplayId::new("missing").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn wire_event_reuse_with_different_payload_conflicts() {
    let log = InMemoryProtocolReplayLog::new();
    log.append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();

    let err = log
        .append_replay(draft("wire-1", "v6", b"different"))
        .await
        .unwrap_err();

    assert!(matches!(err, ProtocolReplayError::Conflict(_)));
}

#[tokio::test]
async fn wire_event_id_is_unique_across_streams() {
    let log = InMemoryProtocolReplayLog::new();
    log.append_replay(draft("wire-1", "v6", b"same"))
        .await
        .unwrap();

    let err = log
        .append_replay(draft_for_stream("thread:t2", "wire-1", "v6", b"same"))
        .await
        .unwrap_err();

    assert!(matches!(err, ProtocolReplayError::Conflict(_)));
}

#[tokio::test]
async fn list_from_cursor_returns_rows_after_cursor() {
    let log = InMemoryProtocolReplayLog::new();
    let first = log
        .append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap();
    log.append_replay(draft("wire-2", "v6", b"two"))
        .await
        .unwrap();

    let page = log
        .list_replay(stream("v6"), Some(first.record.protocol_replay_cursor), 10)
        .await
        .unwrap();

    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].wire_payload_bytes, b"two");
}

#[tokio::test]
async fn list_returns_cursor_expired_for_unknown_cursor() {
    let log = InMemoryProtocolReplayLog::new();
    log.append_replay(draft("wire-1", "v6", b"one"))
        .await
        .unwrap();

    let err = log
        .list_replay(
            stream("v6"),
            Some(ProtocolReplayCursor::new("unknown").unwrap()),
            10,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ProtocolReplayError::CursorExpired(_)));
}

#[tokio::test]
async fn expired_rows_are_omitted_and_their_cursors_expire() {
    let log = InMemoryProtocolReplayLog::new();
    let mut expired = draft("wire-expired", "v6", b"expired");
    expired.expires_at = Some(1);
    let expired = log.append_replay(expired).await.unwrap();
    log.append_replay(draft("wire-live", "v6", b"live"))
        .await
        .unwrap();

    let page = log.list_replay(stream("v6"), None, 10).await.unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].wire_payload_bytes, b"live");

    let err = log
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
async fn source_event_references_are_preserved() {
    let log = InMemoryProtocolReplayLog::new();
    let mut replay = draft("wire-1", "v6", b"one");
    let source_id = CanonicalEventId::new("evt_1").unwrap();
    replay.source_event_ids.push(source_id.clone());
    replay.source_event_cursors.push(SourceEventCursor::new(
        source_id,
        EventScope::thread("t1"),
        EventCursor::new("cur_1").unwrap(),
    ));

    log.append_replay(replay).await.unwrap();
    let page = log.list_replay(stream("v6"), None, 10).await.unwrap();

    assert_eq!(page.records[0].source_event_ids.len(), 1);
    assert_eq!(page.records[0].source_event_cursors.len(), 1);
}

#[tokio::test]
async fn redacted_rows_remain_listable_to_preserve_cursor_continuity() {
    let log = InMemoryProtocolReplayLog::new();
    let mut replay = draft("wire-1", "v6", b"[redacted]");
    replay.redaction_state = ProtocolReplayRedactionState::Redacted;

    log.append_replay(replay).await.unwrap();
    let page = log.list_replay(stream("v6"), None, 10).await.unwrap();

    assert_eq!(page.records.len(), 1);
    assert_eq!(
        page.records[0].redaction_state,
        ProtocolReplayRedactionState::Redacted
    );
    assert_eq!(page.records[0].wire_payload_bytes, b"[redacted]");
}
