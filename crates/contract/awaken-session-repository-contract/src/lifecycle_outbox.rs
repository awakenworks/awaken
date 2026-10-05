//! Drain side of the transactional Session lifecycle outbox.

use async_trait::async_trait;

use super::SessionRepositoryError;
use crate::ManagedLifecycleFact;

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

    /// Every fact not yet retired, in enqueue order.
    ///
    /// Caller: `LifecycleOutboxRelay` drain loop.
    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError>;

    /// Retire one delivered fact by its exact stable id.
    ///
    /// Caller: `LifecycleOutboxRelay` after successful delivery.
    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError>;
}

/// One durable Session store viewed through both of its outbound ports.
///
/// A composition root opens the store once and hands it to the Session
/// application as this object; the application keeps the two ports apart and
/// exposes the outbox only to its delivery consumer. It adds no behaviour a
/// caller could miss.
pub trait SessionStore: super::SessionRepository + LifecycleOutbox {}

impl<T: super::SessionRepository + LifecycleOutbox + ?Sized> SessionStore for T {}
