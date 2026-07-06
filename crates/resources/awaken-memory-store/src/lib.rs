//! Durable memory persistence for the resources plane.
//!
//! The runtime's memory extensions only ever touch the local filesystem and
//! injected prompts (ADR-0038); *durability* — surviving a process restart, being
//! addressable by a stable id — is a resources-plane concern that lives here, so the
//! runtime stays unaware of any store. Two shapes, both file-backed under one root:
//!
//! * [`MemoryBlobStore`] — the id-keyed, mutable byte store behind the ADR-0038
//!   `memory_store` resource family: a session mounts a store by id read-write, the
//!   host harvests the edit back under that id, and — unlike the old in-process map —
//!   the bytes are on disk, so a later process (after a restart) reads them back.
//! * [`memory_scope_root`] — the durable directory an out-of-band *extraction* store
//!   writes its `<slug>.md` files under, derived from the process's storage dir so
//!   memory is governed by the same durable root as every other piece of committed
//!   state instead of a per-process temp dir.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The filename backing a store id. Ids are host-minted (`memstore_<n>`) and thus
/// already safe, but a session can reference an arbitrary `memory_store_id` on the
/// wire — so the id is reduced to a single safe stem here too, and a `get` for a
/// crafted `../` id can never resolve or escape the root.
fn id_filename(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    let stem = if trimmed.is_empty() {
        "memstore".to_string()
    } else {
        trimmed
    };
    format!("{stem}.bin")
}

/// A durable, id-keyed byte store: one file per store id under `root`. Backs the
/// ADR-0038 `memory_store` resource family. Ids minted by [`create`](Self::create)
/// are dense (`memstore_<n>`) and, crucially, the counter is seeded from what is
/// already on disk on [`open`](Self::open), so a fresh process never re-mints an id
/// that already names a persisted store.
pub struct MemoryBlobStore {
    root: PathBuf,
    next: AtomicU64,
}

impl MemoryBlobStore {
    /// Open (creating if absent) the store rooted at `root`, seeding the id counter
    /// past the highest `memstore_<n>` already persisted so ids stay unique across a
    /// restart.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let mut max = 0u64;
        if let Ok(read_dir) = std::fs::read_dir(&root) {
            for entry in read_dir.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    if let Some(n) = stem.strip_prefix("memstore_").and_then(|d| d.parse().ok()) {
                        max = max.max(n);
                    }
                }
            }
        }
        Ok(Self {
            root,
            next: AtomicU64::new(max + 1),
        })
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Mint a new, empty store and return its stable id. The empty file is written
    /// eagerly so the id resolves (to empty content) even before any write-back — and
    /// so it survives a restart as a known-but-empty store rather than a 404.
    pub fn create(&self) -> std::io::Result<String> {
        let id = format!("memstore_{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.put(&id, b"")?;
        Ok(id)
    }

    /// Overwrite the bytes stored under `id` (the harvest write-back path).
    pub fn put(&self, id: &str, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::write(self.root.join(id_filename(id)), bytes)
    }

    /// The bytes stored under `id`, or `None` if no such store exists.
    pub fn get(&self, id: &str) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(id_filename(id))).ok()
    }

    /// Whether a store with `id` exists.
    pub fn exists(&self, id: &str) -> bool {
        self.root.join(id_filename(id)).exists()
    }
}

/// The durable root an out-of-band extraction memory store writes its `<slug>.md`
/// files under, given the process's durable `storage_dir`. Keeping this here (not a
/// per-process temp dir) is what makes extraction memory survive a restart: the same
/// `storage_dir` on a later run yields the same memory directory.
pub fn memory_scope_root(storage_dir: impl AsRef<Path>) -> PathBuf {
    storage_dir.as_ref().join("memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("awaken-memblob-{tag}-{stamp}"))
    }

    #[test]
    fn put_get_roundtrips_and_reopen_reads_the_same_bytes() {
        let root = scratch("roundtrip");
        let id = {
            let store = MemoryBlobStore::open(&root).unwrap();
            let id = store.create().unwrap();
            assert_eq!(
                store.get(&id).as_deref(),
                Some(&b""[..]),
                "created store is empty"
            );
            store.put(&id, b"MARKER").unwrap();
            assert_eq!(store.get(&id).unwrap(), b"MARKER");
            id
        };
        // A fresh process over the same root (a restart) still reads the bytes.
        let reopened = MemoryBlobStore::open(&root).unwrap();
        assert_eq!(
            reopened.get(&id).unwrap(),
            b"MARKER",
            "bytes survive reopen"
        );
        assert!(reopened.exists(&id));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reopen_does_not_remint_an_existing_id() {
        let root = scratch("remint");
        let first = {
            let store = MemoryBlobStore::open(&root).unwrap();
            store.put(&store.create().unwrap(), b"one").unwrap(); // memstore_1
            store.create().unwrap() // memstore_2
        };
        let reopened = MemoryBlobStore::open(&root).unwrap();
        let next = reopened.create().unwrap();
        assert_ne!(next, first, "the counter resumed past what was on disk");
        assert_eq!(next, "memstore_3");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unknown_id_is_none_and_crafted_ids_cannot_escape_root() {
        let root = scratch("escape");
        let store = MemoryBlobStore::open(&root).unwrap();
        assert_eq!(store.get("nope"), None);
        // A path-traversal id is clamped to a single stem under root.
        store.put("../../etc/passwd", b"x").unwrap();
        assert!(root.join("etc-passwd.bin").exists());
        assert!(!root.parent().unwrap().join("passwd.bin").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scope_root_is_under_the_storage_dir() {
        assert_eq!(memory_scope_root("/data"), PathBuf::from("/data/memory"));
    }
}
