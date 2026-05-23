//! In-memory protocol replay log.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayAppendResult, ProtocolReplayCursor, ProtocolReplayDraft, ProtocolReplayError,
    ProtocolReplayId, ProtocolReplayLookup, ProtocolReplayPage, ProtocolReplayReader,
    ProtocolReplayRecord, ProtocolReplayWriter, ProtocolStreamKey,
};
use tokio::sync::RwLock;

#[derive(Debug, Default)]
struct InMemoryProtocolReplayState {
    next_row_seq: u64,
    next_stream_seq: BTreeMap<ProtocolStreamKey, u64>,
    records: BTreeMap<ProtocolReplayId, ProtocolReplayRecord>,
    stream_indexes: BTreeMap<ProtocolStreamKey, Vec<ProtocolReplayId>>,
    cursor_positions: BTreeMap<(ProtocolStreamKey, ProtocolReplayCursor), usize>,
    wire_index: BTreeMap<(String, String, String), ProtocolReplayId>,
}

/// In-memory implementation of protocol replay-log traits.
#[derive(Debug, Clone, Default)]
pub struct InMemoryProtocolReplayLog {
    state: Arc<RwLock<InMemoryProtocolReplayState>>,
}

impl InMemoryProtocolReplayLog {
    /// Create an empty in-memory protocol replay log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ProtocolReplayWriter for InMemoryProtocolReplayLog {
    async fn append_replay(
        &self,
        draft: ProtocolReplayDraft,
    ) -> Result<ProtocolReplayAppendResult, ProtocolReplayError> {
        draft.validate()?;
        let stream = ProtocolStreamKey::new(
            draft.stream_id.clone(),
            draft.protocol.clone(),
            draft.protocol_version.clone(),
        )?;
        let wire_identity = (
            draft.protocol.clone(),
            draft.protocol_version.clone(),
            draft.wire_event_id.clone(),
        );

        let mut state = self.state.write().await;
        if let Some(existing_id) = state.wire_index.get(&wire_identity) {
            let existing = state.records.get(existing_id).cloned().ok_or_else(|| {
                ProtocolReplayError::Integrity(format!(
                    "wire event index points at missing replay row: {}",
                    existing_id.as_str()
                ))
            })?;
            if replay_record_matches_draft(&existing, &draft) {
                return Ok(ProtocolReplayAppendResult { record: existing });
            }
            return Err(ProtocolReplayError::Conflict(format!(
                "wire_event_id reused with different replay row: {}",
                draft.wire_event_id
            )));
        }

        state.next_row_seq += 1;
        let protocol_replay_id = ProtocolReplayId::new(format!("pr_mem_{}", state.next_row_seq))?;
        let stream_seq = {
            let seq = state.next_stream_seq.entry(stream.clone()).or_insert(0);
            *seq += 1;
            *seq
        };
        let cursor = ProtocolReplayCursor::new(format!("prcur_mem_{stream_seq}"))?;
        let record = ProtocolReplayRecord::from_append(
            protocol_replay_id.clone(),
            cursor.clone(),
            crate::current_millis(),
            draft,
        )?;

        let index = state.stream_indexes.entry(stream.clone()).or_default();
        let position = index.len();
        index.push(protocol_replay_id.clone());
        state
            .cursor_positions
            .insert((stream.clone(), cursor), position);
        state.wire_index.insert(
            (
                record.protocol.clone(),
                record.protocol_version.clone(),
                record.wire_event_id.clone(),
            ),
            protocol_replay_id.clone(),
        );
        state.records.insert(protocol_replay_id, record.clone());

        Ok(ProtocolReplayAppendResult { record })
    }
}

#[async_trait]
impl ProtocolReplayReader for InMemoryProtocolReplayLog {
    async fn list_replay(
        &self,
        stream: ProtocolStreamKey,
        from: Option<ProtocolReplayCursor>,
        limit: usize,
    ) -> Result<ProtocolReplayPage, ProtocolReplayError> {
        stream.validate()?;
        let state = self.state.read().await;
        list_locked(&state, &stream, from.as_ref(), limit)
    }
}

#[async_trait]
impl ProtocolReplayLookup for InMemoryProtocolReplayLog {
    async fn load_replay(
        &self,
        protocol_replay_id: &ProtocolReplayId,
    ) -> Result<Option<ProtocolReplayRecord>, ProtocolReplayError> {
        let state = self.state.read().await;
        Ok(state.records.get(protocol_replay_id).cloned())
    }
}

fn replay_record_matches_draft(record: &ProtocolReplayRecord, draft: &ProtocolReplayDraft) -> bool {
    record.stream_id == draft.stream_id
        && record.protocol == draft.protocol
        && record.protocol_version == draft.protocol_version
        && record.projector_version == draft.projector_version
        && record.wire_event_id == draft.wire_event_id
        && record.wire_event_type == draft.wire_event_type
        && record.wire_payload_bytes == draft.wire_payload_bytes
        && record.wire_payload_json == draft.wire_payload_json
        && record.source_event_ids == draft.source_event_ids
        && record.source_event_cursors == draft.source_event_cursors
        && record.redaction_state == draft.redaction_state
        && record.expires_at == draft.expires_at
}

fn list_locked(
    state: &InMemoryProtocolReplayState,
    stream: &ProtocolStreamKey,
    from: Option<&ProtocolReplayCursor>,
    limit: usize,
) -> Result<ProtocolReplayPage, ProtocolReplayError> {
    let ids = state
        .stream_indexes
        .get(stream)
        .cloned()
        .unwrap_or_default();
    let now = crate::current_millis();
    let start = match from {
        Some(cursor) => {
            let position = state
                .cursor_positions
                .get(&(stream.clone(), cursor.clone()))
                .ok_or_else(|| ProtocolReplayError::CursorExpired(cursor.as_str().to_string()))?;
            let Some(record_id) = ids.get(*position) else {
                return Err(ProtocolReplayError::CursorExpired(
                    cursor.as_str().to_string(),
                ));
            };
            let record = state.records.get(record_id).ok_or_else(|| {
                ProtocolReplayError::Integrity(format!(
                    "replay index points at missing row: {record_id:?}"
                ))
            })?;
            if is_expired(record, now) {
                return Err(ProtocolReplayError::CursorExpired(
                    cursor.as_str().to_string(),
                ));
            }
            position.saturating_add(1)
        }
        None => 0,
    };
    let mut records = Vec::new();
    let mut has_more = false;
    for replay_id in ids.iter().skip(start) {
        let record = state.records.get(replay_id).cloned().ok_or_else(|| {
            ProtocolReplayError::Integrity(format!(
                "replay index points at missing row: {replay_id:?}"
            ))
        })?;
        if is_expired(&record, now) {
            continue;
        }
        if records.len() == limit {
            has_more = true;
            break;
        }
        records.push(record);
    }
    let next_cursor = if has_more {
        records
            .last()
            .map(|record| record.protocol_replay_cursor.clone())
    } else {
        None
    };
    Ok(ProtocolReplayPage {
        records,
        next_cursor,
        has_more,
    })
}

fn is_expired(record: &ProtocolReplayRecord, now: u64) -> bool {
    record
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
}
