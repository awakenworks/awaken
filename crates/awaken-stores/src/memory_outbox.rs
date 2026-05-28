//! In-memory outbox store.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::outbox::{
    OutboxEnqueueResult, OutboxError, OutboxMessage, OutboxMessageDraft, OutboxNackOutcome,
    OutboxStatus, OutboxStore,
};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
struct DedupeRecord {
    digest: Vec<u8>,
    outbox_id: String,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InMemoryOutboxState {
    next_id: u64,
    messages: BTreeMap<String, OutboxMessage>,
    dedupe: BTreeMap<(String, String, String), DedupeRecord>,
}

/// In-memory implementation of [`OutboxStore`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryOutboxStore {
    state: Arc<RwLock<InMemoryOutboxState>>,
}

impl InMemoryOutboxStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot internal state for transactional rollback (ADR-0036).
    pub(crate) async fn snapshot_state(&self) -> InMemoryOutboxState {
        self.state.read().await.clone()
    }

    /// Restore previously-snapshotted state.
    pub(crate) async fn restore_state(&self, snapshot: InMemoryOutboxState) {
        *self.state.write().await = snapshot;
    }
}

#[async_trait]
impl OutboxStore for InMemoryOutboxStore {
    async fn enqueue_outbox(
        &self,
        draft: OutboxMessageDraft,
    ) -> Result<OutboxEnqueueResult, OutboxError> {
        draft.validate()?;
        let digest = draft_digest(&draft)?;
        let mut state = self.state.write().await;
        if let Some(dedupe_key) = draft.dedupe_key.as_ref() {
            let identity = (draft.lane.clone(), draft.target.clone(), dedupe_key.clone());
            if let Some(record) = state.dedupe.get(&identity) {
                if record.digest != digest {
                    return Err(OutboxError::Conflict(format!(
                        "outbox dedupe key reused with different input: {dedupe_key}"
                    )));
                }
                let message = state
                    .messages
                    .get(&record.outbox_id)
                    .cloned()
                    .ok_or_else(|| {
                        OutboxError::Io(format!(
                            "outbox dedupe index points at missing message: {}",
                            record.outbox_id
                        ))
                    })?;
                return Ok(OutboxEnqueueResult { message });
            }
        }

        state.next_id += 1;
        let outbox_id = format!("out_mem_{}", state.next_id);
        let message =
            OutboxMessage::from_enqueue(outbox_id.clone(), draft.clone(), current_millis())?;
        if let Some(dedupe_key) = draft.dedupe_key {
            state.dedupe.insert(
                (draft.lane, draft.target, dedupe_key),
                DedupeRecord { digest, outbox_id },
            );
        }
        state
            .messages
            .insert(message.outbox_id.clone(), message.clone());
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
        reject_blank("lane", lane)?;
        reject_blank("target", target)?;
        reject_blank("consumer_id", consumer_id)?;
        let mut state = self.state.write().await;
        let mut ids = state.messages.keys().cloned().collect::<Vec<_>>();
        ids.sort_by_key(|id| {
            state.messages.get(id).map(|message| {
                (
                    message.available_at,
                    message.created_at,
                    message.outbox_id.clone(),
                )
            })
        });
        let mut claimed = Vec::new();
        for id in ids {
            if claimed.len() >= limit {
                break;
            }
            let Some(message) = state.messages.get_mut(&id) else {
                continue;
            };
            if !is_claimable(message, lane, target, now) {
                continue;
            }
            message.status = OutboxStatus::Claimed;
            message.attempt_count = message.attempt_count.saturating_add(1);
            message.claimed_by = Some(consumer_id.to_string());
            message.claim_token = Some(format!(
                "claim_{}_{}",
                uuid::Uuid::now_v7(),
                message.attempt_count
            ));
            message.lease_expires_at = Some(now.saturating_add(lease_ms));
            message.updated_at = now;
            claimed.push(message.clone());
        }
        Ok(claimed)
    }

    async fn ack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<bool, OutboxError> {
        let mut state = self.state.write().await;
        let Some(message) = state.messages.get_mut(outbox_id) else {
            return Ok(false);
        };
        if message.status != OutboxStatus::Claimed
            || message.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(false);
        }
        message.status = OutboxStatus::Delivered;
        message.claimed_by = None;
        message.claim_token = None;
        message.lease_expires_at = None;
        message.updated_at = now;
        Ok(true)
    }

    async fn nack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        error: &str,
        retry_at: u64,
        now: u64,
    ) -> Result<OutboxNackOutcome, OutboxError> {
        let mut state = self.state.write().await;
        let Some(message) = state.messages.get_mut(outbox_id) else {
            return Ok(OutboxNackOutcome::LostClaim);
        };
        if message.status != OutboxStatus::Claimed
            || message.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(OutboxNackOutcome::LostClaim);
        }
        message.last_error = Some(error.to_string());
        message.claimed_by = None;
        message.claim_token = None;
        message.lease_expires_at = None;
        message.updated_at = now;
        if message.attempt_count >= message.max_attempts {
            message.status = OutboxStatus::DeadLetter;
            Ok(OutboxNackOutcome::DeadLettered)
        } else {
            message.status = OutboxStatus::Pending;
            message.available_at = retry_at;
            Ok(OutboxNackOutcome::Requeued)
        }
    }

    async fn list_outbox(
        &self,
        status: Option<OutboxStatus>,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>, OutboxError> {
        let state = self.state.read().await;
        let mut messages = state.messages.values().cloned().collect::<Vec<_>>();
        messages.sort_by_key(|message| {
            (
                message.created_at,
                message.available_at,
                message.outbox_id.clone(),
            )
        });
        Ok(messages
            .into_iter()
            .filter(|message| status.is_none_or(|status| message.status == status))
            .take(limit)
            .collect())
    }
}

fn is_claimable(message: &OutboxMessage, lane: &str, target: &str, now: u64) -> bool {
    message.lane == lane
        && message.target == target
        && matches!(
            message.status,
            OutboxStatus::Pending | OutboxStatus::Claimed
        )
        && message.available_at <= now
        && (message.status == OutboxStatus::Pending
            || message
                .lease_expires_at
                .is_some_and(|expires_at| expires_at <= now))
}

fn draft_digest(draft: &OutboxMessageDraft) -> Result<Vec<u8>, OutboxError> {
    serde_json::to_vec(draft).map_err(|error| OutboxError::Serialization(error.to_string()))
}

fn current_millis() -> u64 {
    crate::current_millis()
}

fn reject_blank(field: &str, value: &str) -> Result<(), OutboxError> {
    if value.trim().is_empty() {
        return Err(OutboxError::Validation(format!("{field} is required")));
    }
    Ok(())
}
