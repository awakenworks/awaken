//! Generic at-least-once outbox data vocabulary.
//!
//! The `OutboxStore` trait (the server/store write+claim surface) moved to
//! `awaken-server-contract`; the runtime only needs these data types
//! (`OutboxError` is named by the commit-coordinator write boundary).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const OUTBOX_LANE_CANONICAL: &str = "canonical";
pub const OUTBOX_LANE_PROTOCOL_REPLAY: &str = "protocol_replay";
pub const OUTBOX_TARGET_PROTOCOL_PROJECTOR: &str = "protocol_projector";
pub const OUTBOX_TARGET_PROTOCOL_FANOUT: &str = "protocol_fanout";
pub const OUTBOX_TARGET_A2A_WEBHOOK: &str = "a2a_webhook";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OutboxError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Claimed,
    Delivered,
    DeadLetter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxMessageDraft {
    pub lane: String,
    pub target: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    #[serde(default)]
    pub available_at: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

impl OutboxMessageDraft {
    pub fn new(
        lane: impl Into<String>,
        target: impl Into<String>,
        payload: Value,
    ) -> Result<Self, OutboxError> {
        let draft = Self {
            lane: lane.into(),
            target: target.into(),
            payload,
            dedupe_key: None,
            available_at: 0,
            max_attempts: default_max_attempts(),
        };
        draft.validate()?;
        Ok(draft)
    }

    pub fn validate(&self) -> Result<(), OutboxError> {
        reject_blank("lane", &self.lane)?;
        reject_blank("target", &self.target)?;
        if let Some(dedupe_key) = &self.dedupe_key {
            reject_blank("dedupe_key", dedupe_key)?;
        }
        if self.max_attempts == 0 {
            return Err(OutboxError::Validation(
                "max_attempts must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxMessage {
    pub outbox_id: String,
    pub lane: String,
    pub target: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub status: OutboxStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl OutboxMessage {
    pub fn from_enqueue(
        outbox_id: String,
        draft: OutboxMessageDraft,
        now: u64,
    ) -> Result<Self, OutboxError> {
        draft.validate()?;
        reject_blank("outbox_id", &outbox_id)?;
        Ok(Self {
            outbox_id,
            lane: draft.lane,
            target: draft.target,
            payload: draft.payload,
            dedupe_key: draft.dedupe_key,
            status: OutboxStatus::Pending,
            available_at: draft.available_at,
            attempt_count: 0,
            max_attempts: draft.max_attempts,
            claimed_by: None,
            claim_token: None,
            lease_expires_at: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEnqueueResult {
    pub message: OutboxMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxNackOutcome {
    Requeued,
    DeadLettered,
    LostClaim,
}

fn default_max_attempts() -> u32 {
    5
}

fn reject_blank(field: &str, value: &str) -> Result<(), OutboxError> {
    if value.trim().is_empty() {
        return Err(OutboxError::Validation(format!("{field} is required")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_rejects_blank_lane() {
        let err = OutboxMessageDraft::new(" ", "target", serde_json::json!({})).unwrap_err();
        assert!(matches!(err, OutboxError::Validation(message) if message.contains("lane")));
    }

    #[test]
    fn message_from_enqueue_initializes_pending_delivery_state() {
        let draft =
            OutboxMessageDraft::new("canonical", "projector", serde_json::json!({})).unwrap();
        let message = OutboxMessage::from_enqueue("out_1".into(), draft, 42).unwrap();
        assert_eq!(message.status, OutboxStatus::Pending);
        assert_eq!(message.attempt_count, 0);
        assert_eq!(message.created_at, 42);
        assert!(message.claim_token.is_none());
    }
}
