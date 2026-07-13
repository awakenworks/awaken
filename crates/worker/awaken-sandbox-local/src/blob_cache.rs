//! `WorkspaceBlobCache` — a workspace-scoped, content-addressed LRU in front of a
//! [`BlobSource`] (awaken-next `WorkspaceBlobCache` parity). The cache key is
//! `(workspace_id, content_id)`, so one tenant's cached bytes can **never** be served
//! to another tenant even if a content id collides — cross-tenant collision
//! isolation. Bounded: past `capacity`, the least-recently-used entry is evicted.
//!
//! It is a transparent decorator: the sandbox providers keep consuming the neutral
//! `BlobSource` port; the cache slots in behind it (dependency-inverted).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract::BlobSource;

type Key = (String, String);

/// The shared LRU store; multiple per-workspace caches can point at one instance so a
/// fleet shares a single bounded cache while staying tenant-isolated by key.
#[derive(Debug)]
pub struct BlobLru {
    map: HashMap<Key, Vec<u8>>,
    /// LRU order, oldest first.
    order: Vec<Key>,
    capacity: usize,
}

impl BlobLru {
    /// A bounded LRU holding at most `capacity` blobs (min 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    fn touch(&mut self, key: &Key) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos);
            self.order.push(k); // move to newest
        }
    }

    fn get(&mut self, key: &Key) -> Option<Vec<u8>> {
        let hit = self.map.get(key).cloned();
        if hit.is_some() {
            self.touch(key);
        }
        hit
    }

    fn insert(&mut self, key: Key, bytes: Vec<u8>) {
        if self.map.insert(key.clone(), bytes).is_none() {
            self.order.push(key);
        } else {
            self.touch(&key);
        }
        while self.order.len() > self.capacity {
            let oldest = self.order.remove(0);
            self.map.remove(&oldest);
        }
    }
}

/// A workspace-scoped view over a shared [`BlobLru`], fronting an inner [`BlobSource`].
pub struct WorkspaceBlobCache {
    workspace_id: String,
    inner: Arc<dyn BlobSource>,
    lru: Arc<Mutex<BlobLru>>,
}

impl WorkspaceBlobCache {
    /// A cache for `workspace_id` with its own bounded LRU of `capacity` blobs.
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        inner: Arc<dyn BlobSource>,
        capacity: usize,
    ) -> Self {
        Self::with_shared(
            workspace_id,
            inner,
            Arc::new(Mutex::new(BlobLru::new(capacity))),
        )
    }

    /// A cache for `workspace_id` over an already-shared LRU (a fleet-wide cache).
    #[must_use]
    pub fn with_shared(
        workspace_id: impl Into<String>,
        inner: Arc<dyn BlobSource>,
        lru: Arc<Mutex<BlobLru>>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            inner,
            lru,
        }
    }
}

#[async_trait]
impl BlobSource for WorkspaceBlobCache {
    async fn get(&self, id: &str) -> Option<Vec<u8>> {
        let key = (self.workspace_id.clone(), id.to_string());
        if let Some(bytes) = self.lru.lock().unwrap().get(&key) {
            return Some(bytes);
        }
        // Miss: fetch from the backing store and cache under the tenant-scoped key.
        let bytes = self.inner.get(id).await?;
        self.lru.lock().unwrap().insert(key, bytes.clone());
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An inner store that counts fetches, so we can assert the cache elides them.
    struct CountingStore {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl BlobSource for CountingStore {
        async fn get(&self, _id: &str) -> Option<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(self.bytes.clone())
        }
    }

    #[tokio::test]
    async fn a_repeated_get_hits_the_cache_not_the_store() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingStore {
            bytes: b"blob".to_vec(),
            calls: calls.clone(),
        });
        let cache = WorkspaceBlobCache::new("ws", inner, 8);
        assert_eq!(cache.get("x").await.unwrap(), b"blob");
        assert_eq!(cache.get("x").await.unwrap(), b"blob");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second get served from cache"
        );
    }

    #[tokio::test]
    async fn the_same_id_in_two_workspaces_never_shares_bytes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner: Arc<dyn BlobSource> = Arc::new(CountingStore {
            bytes: b"shared-hash-bytes".to_vec(),
            calls: calls.clone(),
        });
        let lru = Arc::new(Mutex::new(BlobLru::new(8)));
        let ws_a = WorkspaceBlobCache::with_shared("ws-a", inner.clone(), lru.clone());
        let ws_b = WorkspaceBlobCache::with_shared("ws-b", inner, lru);

        ws_a.get("x").await.unwrap();
        ws_b.get("x").await.unwrap(); // different tenant → separate key → separate fetch
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a tenant's cached blob is never served to another tenant"
        );
    }

    #[tokio::test]
    async fn the_lru_evicts_the_oldest_past_capacity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingStore {
            bytes: b"b".to_vec(),
            calls: calls.clone(),
        });
        let cache = WorkspaceBlobCache::new("ws", inner, 2);
        cache.get("a").await; // [a]
        cache.get("b").await; // [a,b]
        cache.get("c").await; // evicts a → [b,c]
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        cache.get("a").await; // a was evicted → refetch
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        cache.get("c").await; // c still cached → no fetch
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
