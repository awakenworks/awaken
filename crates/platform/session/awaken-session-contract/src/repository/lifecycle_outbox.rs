//! Drain side of the transactional Session lifecycle outbox.

use async_trait::async_trait;

use super::SessionRepositoryError;
use crate::repository::ManagedLifecycleFact;

/// Continuation for enqueue-ordered delivery. Repositories use the stored
/// insertion time and stable fact ID, so the cursor survives row completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleOutboxCursor {
    created_at: String,
    fact_id: String,
}

impl LifecycleOutboxCursor {
    #[must_use]
    pub fn new(created_at: String, fact_id: String) -> Self {
        Self {
            created_at,
            fact_id,
        }
    }

    #[must_use]
    pub fn position(&self) -> (&str, &str) {
        (&self.created_at, &self.fact_id)
    }
}

pub struct LifecycleOutboxBatch {
    facts: Vec<ManagedLifecycleFact>,
    next: Option<LifecycleOutboxCursor>,
}

impl LifecycleOutboxBatch {
    #[must_use]
    pub fn new(facts: Vec<ManagedLifecycleFact>, next: Option<LifecycleOutboxCursor>) -> Self {
        Self { facts, next }
    }

    /// Advance even when delivery fails. The next drain starts from the beginning.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ManagedLifecycleFact>, Option<LifecycleOutboxCursor>) {
        (self.facts, self.next)
    }
}

/// Outbound port over the durable lifecycle outbox.
///
/// Facts are enqueued atomically by
/// [`SessionRepository::create`](super::SessionRepository::create) and
/// [`SessionRepository::commit`](super::SessionRepository::commit); this port
/// only lets a delivery consumer read and retire them, so the aggregate
/// persistence port exposes no queue mechanics. The same durable store
/// implements both ports over one transaction boundary.
#[async_trait]
pub trait LifecycleOutbox: Send + Sync {
    /// Enqueue a lifecycle fact outside an aggregate transaction, idempotently by
    /// stable id.
    ///
    /// Caller: outbox consumers' integration evidence (webhook delivery and
    /// deployment lifecycle tests); aggregate writers use the atomic
    /// `create`/`commit` path instead.
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError>;

    /// Read a bounded batch of pending facts in enqueue order.
    ///
    /// The compatibility implementation can serve older custom adapters.
    /// Production adapters should override it with a bounded keyset query.
    async fn list_pending(
        &self,
        after: Option<&LifecycleOutboxCursor>,
        limit: std::num::NonZeroU32,
    ) -> Result<LifecycleOutboxBatch, SessionRepositoryError> {
        let mut facts = self.pending_lifecycle().await?;
        // Standalone adapters without insertion metadata use the stable fact ID.
        facts.sort_unstable_by(|left, right| left.id.cmp(&right.id));
        let start = after.map_or(0, |after| {
            facts.partition_point(|fact| fact.id <= after.fact_id)
        });
        let facts: Vec<_> = facts
            .into_iter()
            .skip(start)
            .take(limit.get() as usize)
            .collect();
        let next = facts
            .last()
            .filter(|_| facts.len() == limit.get() as usize)
            .map(|fact| LifecycleOutboxCursor::new(String::new(), fact.id.clone()));
        Ok(LifecycleOutboxBatch::new(facts, next))
    }

    /// Reserve one pending fact for delivery. A concurrent replica may return
    /// no claim. Single-consumer adapters can keep the compatibility default.
    async fn claim_lifecycle(
        &self,
        _fact_id: &str,
        _ttl: std::time::Duration,
    ) -> Result<Option<String>, SessionRepositoryError> {
        Ok(Some(String::new()))
    }

    /// Extend only the current, live delivery claim.
    async fn renew_lifecycle_claim(
        &self,
        _fact_id: &str,
        _claim: &str,
        _ttl: std::time::Duration,
    ) -> Result<bool, SessionRepositoryError> {
        Ok(true)
    }

    /// Settle the exact claim. Failure releases it for replay; only success
    /// retires the original fact. A superseded claim has no effect.
    async fn finish_lifecycle_claim(
        &self,
        fact_id: &str,
        _claim: &str,
        delivered: bool,
    ) -> Result<(), SessionRepositoryError> {
        if delivered {
            self.complete_lifecycle(fact_id).await?;
        }
        Ok(())
    }

    /// Every fact not yet retired, for compatibility and verification.
    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError>;

    /// Retire one delivered fact by its exact stable id.
    ///
    /// Caller: `LifecycleOutboxRelay` after successful delivery.
    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError>;
}
