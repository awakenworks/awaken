pub use awaken_runtime_contract::contract::outbox::*;

use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

/// Outbox store contract (ADR-0034 D9/D10).
///
/// `enqueue_outbox` is the protocol-neutral entry point and does NOT share a
/// transaction with the caller's domain writes. Backends with transactional
/// guarantees expose an additional concrete-type entry point that accepts a
/// backend-specific transaction handle:
///
/// - `PostgresStore` provides `enqueue_outbox_in_transaction` to attach an
///   outbox insert to an externally-managed `sqlx::Transaction`. This is the
///   path control-plane writers use to honor ADR-0034 D9's `BEGIN ... update
///   domain row ... append canonical event ... insert outbox ... COMMIT`
///   atomicity requirement.
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
