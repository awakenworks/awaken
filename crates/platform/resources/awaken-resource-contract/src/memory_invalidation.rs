//! Cross-projection cache coherence (ADR-0053 D5).
//!
//! Every sandbox has its own projection and FUSE cache, so a write through one
//! projection must invalidate the same path in every peer cache. The [`MemoryInvalidator`]
//! port is that broadcast: a write publishes
//! `(store, path)`, and each mount's listener drops that path from its LRU (the next
//! read refetches the durable head). [`LocalInvalidator`] is the in-process bus; a
//! distributed transport can implement the same trait without touching the store or
//! FUSE realization.
//!
//! The bus is the in-process implementation of this contract's
//! [`MemoryInvalidator`] port, so the publisher (the Resource application's
//! Memory write decorator, selected by every process that opens the Resources
//! stores, a Coordinator included) and the subscriber (the Worker's Memory
//! mounter) share one type without either linking the other.

use crate::MemoryInvalidator;
use tokio::sync::broadcast;

/// A `(store_id, path)` invalidation — "this path changed; drop it".
pub type Invalidation = (String, String);

/// The in-process invalidation bus (a broadcast). A future distributed transport
/// can implement [`MemoryInvalidator`] over the network.
pub struct LocalInvalidator {
    tx: broadcast::Sender<Invalidation>,
}

impl LocalInvalidator {
    /// A bus buffering up to `capacity` un-drained invalidations per subscriber.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// A receiver a mount drains to drop invalidated paths from its cache.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Invalidation> {
        self.tx.subscribe()
    }

    /// The number of live subscriptions. A mount construction that fails
    /// before it starts a listener leaves this count unchanged, which is how
    /// the mounter's fail-closed construction is observed.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for LocalInvalidator {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl MemoryInvalidator for LocalInvalidator {
    fn publish(&self, store: &str, path: &str) {
        // Err only when there are no live subscribers — nothing to invalidate.
        let _ = self.tx.send((store.to_string(), path.to_string()));
    }
}
