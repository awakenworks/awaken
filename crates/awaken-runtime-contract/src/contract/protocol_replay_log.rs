//! Protocol wire replay log contracts.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::event_store::{CanonicalEventId, EventCursor, EventScope};

/// Errors returned by protocol replay log implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolReplayError {
    /// The provided input violates the replay-log contract.
    #[error("validation error: {0}")]
    Validation(String),
    /// The wire event already exists for the protocol stream with different data.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The requested cursor is outside retained replay history.
    #[error("cursor expired: {0}")]
    CursorExpired(String),
    /// Replay history is missing a row that should still be retained.
    #[error("integrity error: {0}")]
    Integrity(String),
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(String),
    /// A serialization or deserialization error occurred.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Stable protocol replay row identifier assigned by a [`ProtocolReplayWriter`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolReplayId(String);

impl ProtocolReplayId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolReplayError> {
        let value = value.into();
        reject_blank("protocol_replay_id", &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque cursor for a single protocol replay stream.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolReplayCursor(String);

impl ProtocolReplayCursor {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolReplayError> {
        let value = value.into();
        reject_blank("protocol_replay_cursor", &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Redaction state of a replay row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolReplayRedactionState {
    /// The payload is replayable as stored.
    #[default]
    Clear,
    /// The original payload was replaced by a redacted/tombstone payload.
    Redacted,
}

/// Canonical event cursor referenced by a protocol replay row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEventCursor {
    pub event_id: CanonicalEventId,
    pub scope: EventScope,
    pub cursor: EventCursor,
}

impl SourceEventCursor {
    #[must_use]
    pub fn new(event_id: CanonicalEventId, scope: EventScope, cursor: EventCursor) -> Self {
        Self {
            event_id,
            scope,
            cursor,
        }
    }
}

/// ProtocolReplayLog append input. Store-assigned fields are intentionally absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolReplayDraft {
    pub stream_id: String,
    pub protocol: String,
    pub protocol_version: String,
    pub projector_version: String,
    pub wire_event_id: String,
    pub wire_event_type: String,
    pub wire_payload_bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_payload_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_event_ids: Vec<CanonicalEventId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_event_cursors: Vec<SourceEventCursor>,
    #[serde(default)]
    pub redaction_state: ProtocolReplayRedactionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl ProtocolReplayDraft {
    /// Create and validate a replay draft.
    pub fn new(
        stream_id: impl Into<String>,
        protocol: impl Into<String>,
        protocol_version: impl Into<String>,
        projector_version: impl Into<String>,
        wire_event_id: impl Into<String>,
        wire_event_type: impl Into<String>,
        wire_payload_bytes: Vec<u8>,
    ) -> Result<Self, ProtocolReplayError> {
        let draft = Self {
            stream_id: stream_id.into(),
            protocol: protocol.into(),
            protocol_version: protocol_version.into(),
            projector_version: projector_version.into(),
            wire_event_id: wire_event_id.into(),
            wire_event_type: wire_event_type.into(),
            wire_payload_bytes,
            wire_payload_json: None,
            source_event_ids: Vec::new(),
            source_event_cursors: Vec::new(),
            redaction_state: ProtocolReplayRedactionState::default(),
            expires_at: None,
        };
        draft.validate()?;
        Ok(draft)
    }

    /// Validate replay-stream identity and payload presence.
    pub fn validate(&self) -> Result<(), ProtocolReplayError> {
        reject_blank("stream_id", &self.stream_id)?;
        reject_blank("protocol", &self.protocol)?;
        reject_blank("protocol_version", &self.protocol_version)?;
        reject_blank("projector_version", &self.projector_version)?;
        reject_blank("wire_event_id", &self.wire_event_id)?;
        reject_blank("wire_event_type", &self.wire_event_type)?;
        if self.wire_payload_bytes.is_empty() {
            return Err(ProtocolReplayError::Validation(
                "wire_payload_bytes is required".to_string(),
            ));
        }
        Ok(())
    }
}

/// Persisted protocol replay row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolReplayRecord {
    pub protocol_replay_id: ProtocolReplayId,
    pub stream_id: String,
    pub protocol: String,
    pub protocol_version: String,
    pub projector_version: String,
    pub wire_event_id: String,
    pub wire_event_type: String,
    pub wire_payload_bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_payload_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_event_ids: Vec<CanonicalEventId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_event_cursors: Vec<SourceEventCursor>,
    pub protocol_replay_cursor: ProtocolReplayCursor,
    #[serde(default)]
    pub redaction_state: ProtocolReplayRedactionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub created_at: u64,
}

impl ProtocolReplayRecord {
    /// Build a persisted replay row from an accepted draft and store output.
    pub fn from_append(
        protocol_replay_id: ProtocolReplayId,
        protocol_replay_cursor: ProtocolReplayCursor,
        created_at: u64,
        draft: ProtocolReplayDraft,
    ) -> Result<Self, ProtocolReplayError> {
        draft.validate()?;
        Ok(Self {
            protocol_replay_id,
            stream_id: draft.stream_id,
            protocol: draft.protocol,
            protocol_version: draft.protocol_version,
            projector_version: draft.projector_version,
            wire_event_id: draft.wire_event_id,
            wire_event_type: draft.wire_event_type,
            wire_payload_bytes: draft.wire_payload_bytes,
            wire_payload_json: draft.wire_payload_json,
            source_event_ids: draft.source_event_ids,
            source_event_cursors: draft.source_event_cursors,
            protocol_replay_cursor,
            redaction_state: draft.redaction_state,
            expires_at: draft.expires_at,
            created_at,
        })
    }
}

/// Protocol stream selection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolStreamKey {
    pub stream_id: String,
    pub protocol: String,
    pub protocol_version: String,
}

impl ProtocolStreamKey {
    pub fn new(
        stream_id: impl Into<String>,
        protocol: impl Into<String>,
        protocol_version: impl Into<String>,
    ) -> Result<Self, ProtocolReplayError> {
        let key = Self {
            stream_id: stream_id.into(),
            protocol: protocol.into(),
            protocol_version: protocol_version.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn validate(&self) -> Result<(), ProtocolReplayError> {
        reject_blank("stream_id", &self.stream_id)?;
        reject_blank("protocol", &self.protocol)?;
        reject_blank("protocol_version", &self.protocol_version)
    }
}

/// Result returned by an append call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolReplayAppendResult {
    pub record: ProtocolReplayRecord,
}

impl ProtocolReplayAppendResult {
    #[must_use]
    pub fn protocol_replay_cursor(&self) -> &ProtocolReplayCursor {
        &self.record.protocol_replay_cursor
    }
}

/// Paged protocol replay response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolReplayPage {
    pub records: Vec<ProtocolReplayRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<ProtocolReplayCursor>,
    pub has_more: bool,
}

/// Append protocol replay rows.
#[async_trait]
pub trait ProtocolReplayWriter: Send + Sync {
    async fn append_replay(
        &self,
        draft: ProtocolReplayDraft,
    ) -> Result<ProtocolReplayAppendResult, ProtocolReplayError>;
}

/// Read protocol replay history.
#[async_trait]
pub trait ProtocolReplayReader: Send + Sync {
    async fn list_replay(
        &self,
        stream: ProtocolStreamKey,
        from: Option<ProtocolReplayCursor>,
        limit: usize,
    ) -> Result<ProtocolReplayPage, ProtocolReplayError>;
}

/// Load a persisted protocol replay row by durable row identity.
#[async_trait]
pub trait ProtocolReplayLookup: Send + Sync {
    async fn load_replay(
        &self,
        protocol_replay_id: &ProtocolReplayId,
    ) -> Result<Option<ProtocolReplayRecord>, ProtocolReplayError>;
}

/// Full protocol replay-log capability.
pub trait ProtocolReplayLog:
    ProtocolReplayWriter + ProtocolReplayReader + ProtocolReplayLookup
{
}

impl<T> ProtocolReplayLog for T where
    T: ProtocolReplayWriter + ProtocolReplayReader + ProtocolReplayLookup
{
}

fn reject_blank(field: &str, value: &str) -> Result<(), ProtocolReplayError> {
    if value.trim().is_empty() {
        return Err(ProtocolReplayError::Validation(format!(
            "{field} is required"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay_draft() -> ProtocolReplayDraft {
        ProtocolReplayDraft::new(
            "thread:t1",
            "ai-sdk",
            "v6",
            "projector-1",
            "evt_wire_1",
            "agent.message",
            br#"{"type":"agent.message"}"#.to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn draft_requires_wire_payload_bytes() {
        let err = ProtocolReplayDraft::new(
            "thread:t1",
            "ai-sdk",
            "v6",
            "projector-1",
            "evt_wire_1",
            "agent.message",
            Vec::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ProtocolReplayError::Validation(message) if message.contains("wire_payload"))
        );
    }

    #[test]
    fn stream_key_rejects_blank_protocol() {
        let err = ProtocolStreamKey::new("thread:t1", " ", "v6").unwrap_err();
        assert!(
            matches!(err, ProtocolReplayError::Validation(message) if message.contains("protocol"))
        );
    }

    #[test]
    fn persisted_record_preserves_byte_payload_and_cursor() {
        let mut draft = replay_draft();
        draft.wire_payload_json = Some(serde_json::json!({"type":"agent.message"}));
        let record = ProtocolReplayRecord::from_append(
            ProtocolReplayId::new("pr_1").unwrap(),
            ProtocolReplayCursor::new("prcur_1").unwrap(),
            42,
            draft,
        )
        .unwrap();

        assert_eq!(record.protocol_replay_id.as_str(), "pr_1");
        assert_eq!(record.protocol_replay_cursor.as_str(), "prcur_1");
        assert_eq!(record.wire_payload_bytes, br#"{"type":"agent.message"}"#);
        assert_eq!(record.wire_payload_json.unwrap()["type"], "agent.message");
    }

    #[test]
    fn opaque_replay_cursor_roundtrips() {
        let cursor = ProtocolReplayCursor::new("wirecur_opaque").unwrap();
        let encoded = serde_json::to_string(&cursor).unwrap();
        assert_eq!(encoded, "\"wirecur_opaque\"");
        let decoded: ProtocolReplayCursor = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.as_str(), "wirecur_opaque");
    }
}
