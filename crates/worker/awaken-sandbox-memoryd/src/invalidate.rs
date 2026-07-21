//! Cross-host cache coherence (ADR-0053 D5, distributed model).
//!
//! Single-host concurrent mounts share ONE cache (the `MountCoordinator`), so reads
//! are coherent by construction. When the same store is mounted on **different
//! hosts**, each keeps its own cache and a write on host A must invalidate host B's
//! cached path. The [`Invalidator`] port is that broadcast: a write publishes
//! `(store, path)`, and each mount's listener drops that path from its LRU (the next
//! read refetches the durable head). [`LocalInvalidator`] is the in-process bus; a
//! NATS / pg-notify transport swaps in behind the same trait without touching the
//! store or the FUSE.

use std::sync::Arc;

use awaken_memory_store::{MemErr, Memory, MemoryEntry, MemoryFs, MemoryVersion};
use tokio::sync::broadcast;

/// A `(store_id, path)` invalidation — "this path changed; drop it".
pub type Invalidation = (String, String);

/// Announces that a memory path changed so other-host mounts drop it from cache.
pub trait Invalidator: Send + Sync {
    /// Publish that `path` in `store` changed.
    fn publish(&self, store: &str, path: &str);
}

/// The in-process invalidation bus (a broadcast). The distributed transport
/// (NATS / pg-notify) implements [`Invalidator`] the same way over the network.
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
}

impl Default for LocalInvalidator {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl Invalidator for LocalInvalidator {
    fn publish(&self, store: &str, path: &str) {
        // Err only when there are no live subscribers — nothing to invalidate.
        let _ = self.tx.send((store.to_string(), path.to_string()));
    }
}

/// Wrap a [`MemoryFs`] so every mutation publishes an invalidation — the write side
/// of cross-host coherence. Reads pass through unchanged (the durable store is the
/// source of truth; the invalidation only prompts *other* hosts' caches to refetch).
pub struct InvalidatingMemoryFs {
    inner: Arc<dyn MemoryFs>,
    invalidator: Arc<dyn Invalidator>,
}

impl InvalidatingMemoryFs {
    #[must_use]
    pub fn new(inner: Arc<dyn MemoryFs>, invalidator: Arc<dyn Invalidator>) -> Self {
        Self { inner, invalidator }
    }
}

#[async_trait::async_trait]
impl MemoryFs for InvalidatingMemoryFs {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        self.inner.list(store, prefix).await
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        self.inner.get_by_path(store, path).await
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        let memory = self.inner.create(store, path, content).await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }

    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        let memory = self.inner.update(store, id, content, base_sha).await?;
        self.invalidator.publish(store, &memory.path);
        Ok(memory)
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        let memory = self.inner.rename(store, from, to).await?;
        self.invalidator.publish(store, from);
        self.invalidator.publish(store, to);
        Ok(memory)
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        self.inner.delete_by_path(store, path).await?;
        self.invalidator.publish(store, path);
        Ok(())
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        self.inner.list_versions(store).await
    }

    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.inner.redact_version(store, version_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::InMemoryFs;

    #[tokio::test]
    async fn a_mutation_publishes_an_invalidation_others_receive() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryFs::new(Arc::new(InMemoryFs::new()), bus.clone());

        // create → the inner store holds it AND an invalidation is broadcast.
        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(m.content.as_deref(), Some("one"));
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/a.md".to_string())
        );

        // update → invalidation for the memory's path.
        fs.update("s", &m.id, "two", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/a.md".to_string())
        );

        // rename → both the source and destination paths are invalidated.
        fs.rename("s", "/a.md", "/b.md").await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/a.md".to_string())
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/b.md".to_string())
        );

        // delete → invalidation for the removed path.
        fs.delete_by_path("s", "/b.md").await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/b.md".to_string())
        );
    }

    #[tokio::test]
    async fn reads_pass_through_without_invalidating() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryFs::new(Arc::new(InMemoryFs::new()), bus.clone());
        fs.create("s", "/a.md", "x").await.unwrap();
        let _ = rx.recv().await.unwrap(); // drain the create

        // a read publishes nothing.
        assert!(fs.get_by_path("s", "/a.md").await.unwrap().is_some());
        assert!(fs.list("s", "/").await.unwrap().len() == 1);
        assert!(rx.try_recv().is_err(), "reads do not invalidate");
    }

    #[tokio::test]
    async fn a_failed_mutation_publishes_no_invalidation() {
        // The invalidation rides AFTER the inner write's `?`, so a mutation that the
        // store rejects broadcasts nothing — other hosts must not drop a cache entry
        // for a change that never landed.
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryFs::new(Arc::new(InMemoryFs::new()), bus.clone());

        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            ("s".to_string(), "/a.md".to_string()),
            "the successful create did publish"
        );

        // A duplicate create (PathConflict) fails → no publish.
        assert!(fs.create("s", "/a.md", "dup").await.is_err());
        // A CAS update on a stale base sha (Conflict) fails → no publish.
        assert!(fs.update("s", &m.id, "two", "deadbeef").await.is_err());
        // A delete of an id-less path still succeeds (idempotent) — but a create/update
        // that errored above left the bus silent.
        assert!(
            rx.try_recv().is_err(),
            "a rejected mutation broadcasts nothing"
        );
    }
}
