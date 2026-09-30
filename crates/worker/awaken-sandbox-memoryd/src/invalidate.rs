//! Cross-projection cache coherence (ADR-0053 D5).
//!
//! Every sandbox has its own projection and FUSE cache, so a write through one
//! projection must invalidate the same path in every peer cache. The [`Invalidator`]
//! port is that broadcast: a write publishes
//! `(store, path)`, and each mount's listener drops that path from its LRU (the next
//! read refetches the durable head). [`LocalInvalidator`] is the in-process bus; a
//! distributed transport can implement the same trait without touching the store or
//! FUSE realization.

use std::sync::Arc;

use awaken_resource_contract::{
    MemErr, Memory, MemoryActor, MemoryEntry, MemoryRepository, MemoryVersion,
};
use tokio::sync::broadcast;

/// A `(store_id, path)` invalidation — "this path changed; drop it".
pub type Invalidation = (String, String);

/// Announces that a memory path changed so peer projections drop it from cache.
pub trait Invalidator: Send + Sync {
    /// Publish that `path` in `store` changed.
    fn publish(&self, store: &str, path: &str);
}

/// The in-process invalidation bus (a broadcast). A future distributed transport
/// can implement [`Invalidator`] over the network.
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

/// Wrap a [`MemoryRepository`] so every mutation publishes an invalidation — the write side
/// of cross-projection coherence. Reads pass through unchanged (the durable store is
/// the source of truth; invalidation only prompts peer caches to refetch).
pub struct InvalidatingMemoryRepository {
    inner: Arc<dyn MemoryRepository>,
    invalidator: Arc<dyn Invalidator>,
}

impl InvalidatingMemoryRepository {
    #[must_use]
    pub fn new(inner: Arc<dyn MemoryRepository>, invalidator: Arc<dyn Invalidator>) -> Self {
        Self { inner, invalidator }
    }
}

#[async_trait::async_trait]
impl MemoryRepository for InvalidatingMemoryRepository {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        // Retaining a losing proposal changes no mounted head or FUSE cache.
        self.inner.preserve_copy_conflict(store, candidate).await
    }

    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        self.inner.list_copy_conflicts(store).await
    }
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.inner.snapshot_heads(store).await
    }

    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        self.inner.list(store, prefix).await
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        self.inner.get_by_path(store, path).await
    }

    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        // Forward the author: this decorator owns cache invalidation, not
        // attribution, and the inner repository is the one that records it.
        let memory = self.inner.create_as(store, path, content, actor).await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }

    async fn update_head_as(
        &self,
        store: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        let memory = self
            .inner
            .update_head_as(store, base, content, target_path, actor)
            .await?;
        // A path-changing aggregate update can displace another head. Broadcast a
        // store-wide invalidation so peers cannot retain either old path; the
        // resource repository remains unaware of cache/topology concerns.
        if target_path.is_some() {
            self.invalidator.publish(store, "/");
        } else {
            self.invalidator.publish(store, &memory.path);
        }
        Ok(memory)
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        let memory = self
            .inner
            .update_materialized(store, path, base, content)
            .await?;
        self.invalidator.publish(store, path);
        Ok(memory)
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        let memory = self.inner.rename(store, from, to).await?;
        self.invalidator.publish(store, from);
        self.invalidator.publish(store, to);
        Ok(memory)
    }

    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        self.inner.delete_by_path_as(store, path, actor).await?;
        self.invalidator.publish(store, path);
        Ok(())
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let deleted = self
            .inner
            .delete_if_match_as(store, path, base, actor)
            .await?;
        if deleted {
            self.invalidator.publish(store, path);
        }
        Ok(deleted)
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        self.inner.list_versions(store).await
    }

    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.inner.redact_version_as(store, version_id, actor).await
    }

    async fn purge_store(
        &self,
        store: &str,
    ) -> Result<awaken_resource_contract::MemoryPurgeSummary, MemErr> {
        let summary = self.inner.purge_store(store).await?;
        self.invalidator.publish(store, "/");
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::VolatileMemoryRepository;

    #[tokio::test]
    async fn materialized_update_invalidates_only_after_exact_location_admission() {
        // MP-I: exact path => commit plus one path notification; renamed path
        // with identical bytes => conflict plus zero notifications. No timeout
        // stands in for absence: try_recv must return Empty on the live bus.
        let inner = Arc::new(VolatileMemoryRepository::new(
            awaken_lease_contract::system_clock(),
        ));
        let base = inner.create("s", "/a", "base").await.unwrap();
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(inner.clone(), bus);
        fs.update_materialized("s", "/a", &base.generation(), "edit")
            .await
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), ("s".into(), "/a".into()));
        inner.rename("s", "/a", "/b").await.unwrap();
        assert!(matches!(
            fs.update_materialized("s", "/a", &base.generation(), "edit")
                .await,
            Err(MemErr::Conflict { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    /// Attribution cause/effect graph: C1 a caller states an author on each
    /// mutation that records one; C2 the mutation reaches the inner repository
    /// through this cache decorator. Effect: E1 the author reaches the version
    /// history unchanged. Decision rule: A1=C1+C2 => E1 for create, update and
    /// delete alike.
    ///
    /// This decorator forwarded only the conditional delete, so every other
    /// mutation lowered to the unattributed spelling and the author was lost
    /// before the store ever saw it. The five attributed methods are required
    /// now, which is what makes a missing forward a compile error; this test
    /// pins the behaviour that the requirement exists to protect.
    #[tokio::test]
    async fn every_forwarded_mutation_keeps_the_author_the_caller_stated() {
        let inner = Arc::new(VolatileMemoryRepository::new(
            awaken_lease_contract::system_clock(),
        ));
        let bus = Arc::new(LocalInvalidator::new(16));
        let fs = InvalidatingMemoryRepository::new(inner.clone(), bus);
        let actor = MemoryActor::UserActor {
            user_id: "user-7".into(),
        };

        let created = fs
            .create_as("s", "/a.md", "one", Some(&actor))
            .await
            .unwrap();
        let updated = fs
            .update_head_as("s", &created.generation(), "two", None, Some(&actor))
            .await
            .unwrap();
        fs.delete_by_path_as("s", &updated.path, Some(&actor))
            .await
            .unwrap();

        let versions = inner.list_versions("s").await.unwrap();
        assert_eq!(
            versions.len(),
            3,
            "A1 create, update and delete each recorded"
        );
        for version in &versions {
            assert_eq!(
                version.created_by.as_ref(),
                Some(&actor),
                "A1/E1 {:?} lost the author",
                version.operation
            );
        }
    }

    #[tokio::test]
    async fn a_mutation_publishes_an_invalidation_others_receive() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(
                awaken_lease_contract::system_clock(),
            )),
            bus.clone(),
        );

        // create → the inner store holds it AND an invalidation is broadcast.
        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(m.content.as_deref(), Some("one"));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/a.md".to_string())
        );

        // update → invalidation for the memory's path.
        let updated = fs.update("s", &m.generation(), "two").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/a.md".to_string())
        );

        // A combined content/path update may also displace a destination, so it
        // emits the store-root sentinel consumed as a full cache clear.
        fs.update_head("s", &updated.generation(), "three", Some("/b.md"))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/".to_string())
        );

        // rename → both the source and destination paths are invalidated.
        fs.rename("s", "/b.md", "/c.md").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/b.md".to_string())
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/c.md".to_string())
        );

        // delete → invalidation for the removed path.
        fs.delete_by_path("s", "/c.md").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/c.md".to_string())
        );
    }

    #[tokio::test]
    async fn reads_pass_through_without_invalidating() {
        let bus = Arc::new(LocalInvalidator::new(16));
        let mut rx = bus.subscribe();
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(
                awaken_lease_contract::system_clock(),
            )),
            bus.clone(),
        );
        fs.create("s", "/a.md", "x").await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for rx.recv()")
            .unwrap(); // drain the create

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
        let fs = InvalidatingMemoryRepository::new(
            Arc::new(VolatileMemoryRepository::new(
                awaken_lease_contract::system_clock(),
            )),
            bus.clone(),
        );

        let m = fs.create("s", "/a.md", "one").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()")
                .unwrap(),
            ("s".to_string(), "/a.md".to_string()),
            "the successful create did publish"
        );

        // A duplicate create (PathConflict) fails → no publish.
        assert!(fs.create("s", "/a.md", "dup").await.is_err());
        // A CAS update on a stale generation (Conflict) fails → no publish.
        let head = m.generation();
        let stale = awaken_resource_contract::MemoryGeneration::new(
            head.memory_id(),
            head.version(),
            "deadbeef",
        );
        assert!(fs.update("s", &stale, "two").await.is_err());
        // A delete of an id-less path still succeeds (idempotent) — but a create/update
        // that errored above left the bus silent.
        assert!(
            rx.try_recv().is_err(),
            "a rejected mutation broadcasts nothing"
        );
    }
}
