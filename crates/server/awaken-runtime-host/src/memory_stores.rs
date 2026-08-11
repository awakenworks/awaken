//! `MemoryStores` — the Resources context's Memory content backends (ADR-0038/0053).
//!
//! Owns the one path-addressed CAS [`MemoryRepository`] used by the Memory API, sandbox
//! mounts, recall, and extraction. Product startup injects the selected
//! repository; the local opener exists only for tests and scenario fixtures.
//!
//! Resource definition/configuration/lifecycle lives in the platform
//! `ResourceCatalog`, injected at the server process startup. The runtime host owns
//! only content backends; it does not own an authorization policy or a second identity
//! registry.

#[cfg(any(test, feature = "test-support"))]
use std::path::Path;
use std::sync::Arc;

use awaken_resource_contract::MemoryRepository;

/// The Resources context's Memory content backend. See the module docs for why
/// identity/configuration is kept out.
pub(crate) struct MemoryStores {
    /// Durable, path-addressed memory files (ADR-0053): many memories, each at a path
    /// with a `content_sha256` + monotonic version, updated under compare-and-swap.
    fs: Arc<dyn MemoryRepository>,
}

impl MemoryStores {
    pub(crate) fn with_repository(fs: Arc<dyn MemoryRepository>) -> Self {
        Self { fs }
    }

    /// Test-support opener under one storage-dir durability rule (`Some` → durable
    /// under the dir, `None` → ephemeral per-process).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn open(store_dir: Option<&Path>) -> Self {
        // ADR-0053 path-addressed memory files persist alongside, under the same
        // durability rule. Backed by the SQLite store so rename-replace and CAS are
        // **crash-atomic** (one transaction) — the plain-file backend is only no-loss
        // (a crash mid-rename can leave a transient duplicate source).
        let fs: Arc<dyn MemoryRepository> = Arc::new(match store_dir {
            Some(dir) => {
                // Keep the original physical filename for an in-place upgrade; it is
                // storage layout, not the public/domain name of the repository port.
                let db = dir.join("memory_fs.db");
                awaken_memory_store::SqliteMemoryRepository::open(
                    db.to_str().expect("memory-fs db path is valid UTF-8"),
                )
                .expect("open durable memory-fs sqlite store")
            }
            // No durable dir → an ephemeral in-memory database (dies with the process).
            None => awaken_memory_store::SqliteMemoryRepository::open_in_memory()
                .expect("open ephemeral memory-fs sqlite store"),
        });
        Self { fs }
    }

    /// The path-addressed CAS store backing the `/memories` endpoints.
    pub(crate) fn fs(&self) -> &Arc<dyn MemoryRepository> {
        &self.fs
    }

    pub(crate) fn fs_handle(&self) -> Arc<dyn MemoryRepository> {
        self.fs.clone()
    }
}
