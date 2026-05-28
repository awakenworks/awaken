//! Generic at-least-once outbox contract.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;

use super::scope::{ScopeId, scoped_key, unscoped_key};

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

#[derive(Clone)]
pub struct ScopedOutboxStore {
    inner: Arc<dyn OutboxStore>,
    scope_id: ScopeId,
}

impl ScopedOutboxStore {
    pub fn new(inner: Arc<dyn OutboxStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn OutboxStore {
        self.inner.as_ref()
    }

    fn scoped(&self, value: &str) -> String {
        scoped_key(&self.scope_id, value)
    }

    fn unscoped<'a>(&self, value: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, value)
    }

    fn encode_draft(&self, mut draft: OutboxMessageDraft) -> OutboxMessageDraft {
        draft.lane = self.scoped(&draft.lane);
        draft.dedupe_key = draft.dedupe_key.as_deref().map(|key| self.scoped(key));
        draft
    }

    fn decode_message(&self, mut message: OutboxMessage) -> Option<OutboxMessage> {
        message.lane = self.unscoped(&message.lane)?.to_string();
        message.dedupe_key = message
            .dedupe_key
            .as_deref()
            .map(|key| self.unscoped(key).map(str::to_string))
            .unwrap_or(None);
        Some(message)
    }
}

/// Outbox store contract (ADR-0034 D9/D10).
///
/// `enqueue_outbox` is the protocol-neutral entry point and does NOT share a
/// transaction with the caller's domain writes. Backends with transactional
/// guarantees expose an additional concrete-type entry point that accepts a
/// backend-specific transaction handle:
///
/// - [`crate::PostgresStore`] provides `enqueue_outbox_in_transaction` to
///   attach an outbox insert to an externally-managed `sqlx::Transaction`.
///   This is the path control-plane writers use to honor ADR-0034 D9's
///   `BEGIN ... update domain row ... append canonical event ... insert outbox
///   ... COMMIT` atomicity requirement.
/// - The canonical EventStore append paths (Postgres) already enqueue the
///   canonical outbox row within the same EventStore transaction, so callers
///   that only need EventStore + outbox atomicity get it for free via
///   `EventWriter::append`.
/// - Implementations whose database is different from the canonical event
///   backend can ONLY provide eventually-consistent canonical→outbox writes;
///   the deployment must accept the eventual-consistency profile that
///   ADR-0034 documents.
#[async_trait]
pub trait OutboxStore: Send + Sync {
    /// Enqueue an outbox row in the backend's own transaction. Use the
    /// concrete-type transactional method (see trait docs) when the caller
    /// needs to share a transaction with other writes.
    async fn enqueue_outbox(
        &self,
        draft: OutboxMessageDraft,
    ) -> Result<OutboxEnqueueResult, OutboxError>;

    async fn claim_outbox(
        &self,
        lane: &str,
        target: &str,
        limit: usize,
        lease_ms: u64,
        consumer_id: &str,
        now: u64,
    ) -> Result<Vec<OutboxMessage>, OutboxError>;

    async fn ack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<bool, OutboxError>;

    async fn nack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        error: &str,
        retry_at: u64,
        now: u64,
    ) -> Result<OutboxNackOutcome, OutboxError>;

    async fn list_outbox(
        &self,
        status: Option<OutboxStatus>,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>, OutboxError>;
}

#[async_trait]
impl OutboxStore for ScopedOutboxStore {
    async fn enqueue_outbox(
        &self,
        draft: OutboxMessageDraft,
    ) -> Result<OutboxEnqueueResult, OutboxError> {
        let result = self.inner.enqueue_outbox(self.encode_draft(draft)).await?;
        let message = self.decode_message(result.message).ok_or_else(|| {
            OutboxError::Io("scoped outbox store returned a message outside its scope".into())
        })?;
        Ok(OutboxEnqueueResult { message })
    }

    async fn claim_outbox(
        &self,
        lane: &str,
        target: &str,
        limit: usize,
        lease_ms: u64,
        consumer_id: &str,
        now: u64,
    ) -> Result<Vec<OutboxMessage>, OutboxError> {
        Ok(self
            .inner
            .claim_outbox(
                &self.scoped(lane),
                target,
                limit,
                lease_ms,
                consumer_id,
                now,
            )
            .await?
            .into_iter()
            .filter_map(|message| self.decode_message(message))
            .collect())
    }

    async fn ack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<bool, OutboxError> {
        let Some(message) = self
            .list_outbox(Some(OutboxStatus::Claimed), usize::MAX)
            .await?
            .into_iter()
            .find(|message| message.outbox_id == outbox_id)
        else {
            return Ok(false);
        };
        self.inner
            .ack_outbox(&message.outbox_id, claim_token, now)
            .await
    }

    async fn nack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        error: &str,
        retry_at: u64,
        now: u64,
    ) -> Result<OutboxNackOutcome, OutboxError> {
        let Some(message) = self
            .list_outbox(Some(OutboxStatus::Claimed), usize::MAX)
            .await?
            .into_iter()
            .find(|message| message.outbox_id == outbox_id)
        else {
            return Ok(OutboxNackOutcome::LostClaim);
        };
        self.inner
            .nack_outbox(&message.outbox_id, claim_token, error, retry_at, now)
            .await
    }

    async fn list_outbox(
        &self,
        status: Option<OutboxStatus>,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>, OutboxError> {
        Ok(self
            .inner
            .list_outbox(status, usize::MAX)
            .await?
            .into_iter()
            .filter_map(|message| self.decode_message(message))
            .take(limit)
            .collect())
    }
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
