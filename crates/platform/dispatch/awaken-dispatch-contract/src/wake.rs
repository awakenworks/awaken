// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: One existing best-effort hint port; it owns no queue, claim, Clock or delivery decision.
// @formal reason: Local/PG/NATS implementations and bounded publication/poll recovery stay at their original owners.
//! The canonical non-authoritative dispatch wake contract.

use crate::DispatchError;
use async_trait::async_trait;

/// A best-effort cross-node wake hint.
#[async_trait]
pub trait WakeSignal: Send + Sync {
    /// Publish a wake hint. Best-effort: a failure or a lost hint is tolerable.
    async fn publish(&self) -> Result<(), DispatchError>;

    /// Wait for the next wake hint.
    async fn wait(&self);
}
