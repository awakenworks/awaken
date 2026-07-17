//! `MemoryStores` — the memory resource plane's *content* backends (ADR-0038/0053).
//!
//! Groups the two memory content stores that were flat on [`crate::SharedHost`] behind
//! one type that owns their single construction invariant: both follow one storage-dir
//! durability rule — a dir → durable (survive a restart), none → ephemeral
//! (per-process). `blob` is the id-keyed read-write [`MemoryBlobStore`] (a mounted
//! store's bytes); `fs` is the path-addressed CAS [`MemoryFs`] backing the `/memories`
//! endpoints.
//!
//! The memory-store *identity* registry (`MemoryStoreRegistry`) is deliberately NOT
//! here: it is a control-plane concern that lives beside its true siblings
//! (`McpStore`, `WebhookStore`, …) in `awaken-config-resolver`, is injected rather than
//! storage-dir-constructed, and has a different lifecycle — so it stays a separate
//! `SharedHost` field. The cohesion axis is identity-vs-content, not memory-vs-other.

use std::path::Path;
use std::sync::Arc;

use awaken_memory_store::{MemoryBlobStore, MemoryFs};

/// The memory resource plane's content backends, governed by one storage-dir
/// durability rule. See the module docs for why identity is kept out.
pub(crate) struct MemoryStores {
    /// Mutable, id-keyed memory stores (ADR-0038 MemoryStore family): a session mounts
    /// a store's bytes read-write and the host harvests them back after a turn.
    blob: Arc<dyn MemoryBlobStore>,
    /// Durable, path-addressed memory files (ADR-0053): many memories, each at a path
    /// with a `content_sha256` + monotonic version, updated under compare-and-swap.
    fs: Arc<dyn MemoryFs>,
}

impl MemoryStores {
    /// Open the content stores under one storage-dir durability rule (`Some` → durable
    /// under the dir, `None` → ephemeral per-process).
    pub(crate) fn open(store_dir: Option<&Path>) -> Self {
        // The ADR-0038 memory_store family persists under the storage dir when set so a
        // harvested write-back outlives the process; otherwise a per-process temp dir
        // (unit tests / ephemeral use) keeps it in-run only.
        let memory_store_root = match store_dir {
            Some(dir) => dir.join("memory_stores"),
            // Ephemeral: a per-instance dir (pid + a process-local counter), so two
            // hosts in one process (parallel unit tests) never share a memory store.
            None => {
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::env::temp_dir().join(format!("awaken-memstore-{}-{n}", std::process::id()))
            }
        };
        let blob: Arc<dyn MemoryBlobStore> = Arc::new(
            awaken_memory_store::FsMemoryBlobStore::open(&memory_store_root)
                .expect("open durable memory-store root"),
        );
        // ADR-0053 path-addressed memory files persist alongside, under the same
        // durability rule. Backed by the SQLite store so rename-replace and CAS are
        // **crash-atomic** (one transaction) — the plain-file backend is only no-loss
        // (a crash mid-rename can leave a transient duplicate source).
        let fs: Arc<dyn MemoryFs> = Arc::new(match store_dir {
            Some(dir) => {
                let db = dir.join("memory_fs.db");
                awaken_memory_store::SqliteMemoryFs::open(
                    db.to_str().expect("memory-fs db path is valid UTF-8"),
                )
                .expect("open durable memory-fs sqlite store")
            }
            // No durable dir → an ephemeral in-memory database (dies with the process).
            None => awaken_memory_store::SqliteMemoryFs::open_in_memory()
                .expect("open ephemeral memory-fs sqlite store"),
        });
        Self { blob, fs }
    }

    /// The id-keyed read-write blob store (mounted memory content).
    pub(crate) fn blob(&self) -> &Arc<dyn MemoryBlobStore> {
        &self.blob
    }

    /// The path-addressed CAS store backing the `/memories` endpoints.
    pub(crate) fn fs(&self) -> &Arc<dyn MemoryFs> {
        &self.fs
    }
}
