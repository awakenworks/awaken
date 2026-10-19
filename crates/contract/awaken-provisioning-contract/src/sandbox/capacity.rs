// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Declares the backend-erased lifecycle of never-used Sandbox capacity a composition root
// @formal reason: prewarms and drains; this is a product-requirement classification, not a formal-proof claim.
//! Never-used Sandbox capacity, held by the composition that opted into it.

use async_trait::async_trait;

use super::SandboxError;
use crate::spec::SandboxSpec;

/// Backend-erased lifecycle for never-used Sandbox capacity.
///
/// This is deliberately separate from the provider: every provider can realize
/// a Session Sandbox, while only a deployment that opted into a warm pool owns
/// pre-created capacity. A composition root keeps this handle long enough to
/// prewarm before advertising readiness and to drain the unused capacity during
/// shutdown.
#[async_trait]
pub trait SandboxCapacity: Send + Sync {
    /// Ensure at least `target` ready, never-used Sandboxes exist for `spec`'s
    /// exact creation shape. Returns the resulting ready count. Non-poolable specs
    /// (currently any spec with mounts) return zero without creating anything.
    async fn prewarm_to(&self, spec: &SandboxSpec, target: usize) -> Result<usize, SandboxError>;

    /// Current ready, never-used capacity for one exact shape. Receipts are
    /// derived from this value so an eviction or Session checkout cannot leave a
    /// stale positive placement hint.
    fn ready_capacity(&self, spec: &SandboxSpec) -> usize;

    /// Dispose unused capacity for one exact shape. This is used when the
    /// authoritative current Environment catalog no longer desires an older
    /// revision; active Session Sandboxes have already left capacity.
    async fn discard_shape(&self, spec: &SandboxSpec);

    /// Stop replenishment and dispose all never-used capacity. Active Session
    /// Sandboxes have already left the pool and are not affected.
    async fn shutdown_capacity(&self);
}
