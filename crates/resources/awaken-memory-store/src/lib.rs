//! Durable memory persistence for the resources plane.
//!
//! The [`MemoryBlobStore`] port (mirroring awaken-file-store's `FileStore`) with
//! pluggable backends: [`InMemoryBlobStore`], [`FsMemoryBlobStore`] (a byte-file per
//! id on disk), and — feature-gated — `SqliteMemoryBlobStore` / `PgMemoryBlobStore`
//! over one portable bundle. It backs the ADR-0038 `memory_store` resource family: a
//! session mounts a store by id read-write, the host harvests the edit back under
//! that id, and — on postgres — the bytes are shared across nodes. Ids are host-minted
//! and dense (`memstore_<n>`).
//!
//! [`memory_scope_root`] (the extraction-memory directory) stays a filesystem helper
//! here — a separate, directory-shaped store the runtime's memory extension writes to
//! directly, out of scope for the id-keyed blob port.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PgMemoryBlobStore, PgStoreError};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use schema::{BUNDLE_ID, memory_store_bundle};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteMemoryBlobStore, StoreError};

/// The filename backing a store id. Ids minted by [`MemoryBlobStore::create`] are
/// safe (`memstore_<n>`), but a session can reference an arbitrary id on the wire — so
/// the id is reduced to a single safe stem, and a `get` for a crafted `../` id can
/// never resolve or escape the root.
fn id_filename(id: &str) -> String {
    format!("{}.bin", sanitize_stem(id))
}

/// Reduce `name` to a safe single stem: keep alphanumerics, `-`, `_`; map every other
/// run to a single `-`; never empty. A `put` cannot escape the root.
fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memstore".to_string()
    } else {
        trimmed
    }
}

/// A memory-store failure.
#[derive(Debug, thiserror::Error)]
pub enum MemoryStoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// A durable, id-keyed, workspace-scoped byte store: one blob per id. Backs the
/// ADR-0038 `memory_store` resource family. `create` mints a dense, globally-unique
/// `memstore_<n>` id and writes it empty so it resolves before any write-back. Async
/// so a network-DB backend fits; the filesystem/in-memory backends satisfy it
/// trivially. Mirrors awaken-file-store's `FileStore`.
#[async_trait]
pub trait MemoryBlobStore: Send + Sync {
    /// Mint a new, empty store and return its stable id.
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError>;
    /// Overwrite the bytes under `id` (the harvest write-back path).
    async fn put(&self, workspace_id: &str, id: &str, bytes: &[u8])
    -> Result<(), MemoryStoreError>;
    /// The bytes under `id`, or `None` if no such store exists.
    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError>;
    /// Whether a store with `id` exists.
    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError>;
}

/// In-memory [`MemoryBlobStore`] (tests / ephemeral single-process).
#[derive(Default)]
pub struct InMemoryBlobStore {
    // workspace_id → (id → bytes)
    inner: Mutex<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    next: AtomicU64,
}

impl InMemoryBlobStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MemoryBlobStore for InMemoryBlobStore {
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError> {
        let id = format!("memstore_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        self.inner
            .lock()
            .unwrap()
            .entry(workspace_id.to_string())
            .or_default()
            .insert(id.clone(), Vec::new());
        Ok(id)
    }

    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), MemoryStoreError> {
        self.inner
            .lock()
            .unwrap()
            .entry(workspace_id.to_string())
            .or_default()
            .insert(sanitize_stem(id), bytes.to_vec());
        Ok(())
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .and_then(|ws| ws.get(&sanitize_stem(id)).cloned()))
    }

    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .is_some_and(|ws| ws.contains_key(&sanitize_stem(id))))
    }
}

/// Filesystem [`MemoryBlobStore`]: `<root>/<workspace>/<id>.bin`, one file per blob.
/// The id counter is seeded from what is already on disk at [`open`](Self::open), so a
/// fresh process never re-mints an id that already names a persisted store. `open` is
/// synchronous (one-time directory scan); the per-blob operations are async.
pub struct FsMemoryBlobStore {
    root: PathBuf,
    next: AtomicU64,
}

impl FsMemoryBlobStore {
    /// Open (creating if absent) the store rooted at `root`, seeding the id counter
    /// past the highest `memstore_<n>` already persisted anywhere under it.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let mut max = 0u64;
        // Ids are globally unique; scan every workspace subdir for the highest n.
        if let Ok(workspaces) = std::fs::read_dir(&root) {
            for ws in workspaces.flatten() {
                if let Ok(files) = std::fs::read_dir(ws.path()) {
                    for entry in files.flatten() {
                        if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str())
                            && let Some(n) =
                                stem.strip_prefix("memstore_").and_then(|d| d.parse().ok())
                        {
                            max = max.max(n);
                        }
                    }
                }
            }
        }
        Ok(Self {
            root,
            next: AtomicU64::new(max),
        })
    }

    /// The store's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ws_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join(sanitize_stem(workspace_id))
    }
}

#[async_trait]
impl MemoryBlobStore for FsMemoryBlobStore {
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError> {
        let id = format!("memstore_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        // Write eagerly so the id resolves (to empty) even before any write-back.
        self.put(workspace_id, &id, b"").await?;
        Ok(id)
    }

    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), MemoryStoreError> {
        let dir = self.ws_dir(workspace_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| MemoryStoreError::Io(e.to_string()))?;
        tokio::fs::write(dir.join(id_filename(id)), bytes)
            .await
            .map_err(|e| MemoryStoreError::Io(e.to_string()))
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError> {
        let path = self.ws_dir(workspace_id).join(id_filename(id));
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemoryStoreError::Io(e.to_string())),
        }
    }

    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError> {
        Ok(self.ws_dir(workspace_id).join(id_filename(id)).exists())
    }
}

/// The durable root an out-of-band extraction memory store writes its `<slug>.md`
/// files under, given the process's durable `storage_dir`. Keeping this here (not a
/// per-process temp dir) is what makes extraction memory survive a restart: the same
/// `storage_dir` on a later run yields the same memory directory. (Directory-shaped
/// store; not part of the id-keyed blob port.)
pub fn memory_scope_root(storage_dir: impl AsRef<Path>) -> PathBuf {
    storage_dir.as_ref().join("memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crafted_ids_cannot_escape_root() {
        let root = std::env::temp_dir().join(format!("awaken-memstore-esc-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let store = FsMemoryBlobStore::open(&root).unwrap();
        store.put("ws", "../../etc/passwd", b"x").await.unwrap();
        assert!(root.join("ws").join("etc-passwd.bin").exists());
        assert!(!root.parent().unwrap().join("passwd.bin").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
