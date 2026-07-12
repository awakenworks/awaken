//! Path-addressed, CAS memory model (ADR-0053, D1).
//!
//! The [`MemoryFs`] port projects a memory store as a set of **path-addressed
//! memory files**, each carrying a `content_sha256`, a monotonic per-path
//! `version`, and real create/update timestamps — the model a write-through FUSE
//! mount ([ADR-0053] D2) needs to give an LLM native `read`/`write`/`edit`/`grep`
//! semantics over `/mnt/memory/{store}/*`. Unlike the id-keyed [`MemoryBlobStore`]
//! (one opaque blob per store), a store here holds many files with **per-file
//! optimistic concurrency**: [`MemoryFs::update`] is a compare-and-swap on the base
//! sha, so two writers never silently clobber each other.
//!
//! Backends: [`InMemoryFs`] (tests / ephemeral) and [`FsMemoryFs`] (a durable JSON
//! record per memory). Sqlite/postgres are a later slice (ADR-0053 P4).
//!
//! [`MemoryBlobStore`]: crate::MemoryBlobStore

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard cap on a single memory's content (Anthropic parity, ADR-0057). Enforced on
/// every create/update before any allocation.
pub const MAX_MEMORY_BYTES: usize = 102_400;
/// Hard cap on a memory path.
pub const MAX_PATH_BYTES: usize = 1024;

/// One path-addressed memory. `content` is present on `get`/`create`/`update`,
/// absent on listings. `created`/`updated` are Unix-epoch nanoseconds so a FUSE
/// `getattr` can report faithful timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub path: String,
    pub content_sha256: String,
    pub content_size: u64,
    /// Monotonic per-path version: 1 on create, +1 on each write. The cache-validation
    /// oracle for the distributed model (ADR-0053 D5) and a diagnostic.
    pub version: u64,
    pub created_unix_nanos: u128,
    pub updated_unix_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// A directory-listing entry (no content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub path: String,
    pub content_sha256: String,
    pub content_size: u64,
    pub version: u64,
    pub updated_unix_nanos: u128,
}

/// A [`MemoryFs`] failure.
#[derive(Debug, thiserror::Error)]
pub enum MemErr {
    #[error("memory not found: {0}")]
    NotFound(String),
    /// Optimistic-concurrency failure: the base sha no longer matches. `current`
    /// carries the server's live memory (sha + content) for diagnostics; the caller
    /// must not clobber.
    #[error("cas conflict on {}: base sha stale", .current.path)]
    Conflict { current: Box<Memory> },
    #[error("path already exists: {0}")]
    PathConflict(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("content exceeds {MAX_MEMORY_BYTES} bytes")]
    TooLarge,
    #[error("storage: {0}")]
    Storage(String),
}

/// A path-addressed, CAS memory store — the seam a write-through FUSE mount calls
/// (ADR-0053). All methods are store-scoped by an opaque, globally-unique `store` id.
#[async_trait]
pub trait MemoryFs: Send + Sync {
    /// Memories whose path is at or under `prefix` (`"/"` or `""` = all).
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    /// The memory at `path`, or `None`.
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    /// Create a memory at `path`. `PathConflict` if it already exists.
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr>;
    /// Compare-and-swap the content of memory `id`: succeeds only if the stored sha
    /// equals `base_sha` (or the new content is already current — idempotent). On a
    /// mismatch returns [`MemErr::Conflict`] carrying the live memory; never clobbers.
    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr>;
    /// Move `from` → `to` atomically, **replacing** `to` if it exists (POSIX rename
    /// semantics). The memory keeps its id (so an open FUSE fd stays valid) and bumps
    /// its version.
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr>;
    /// Delete the memory at `path` (idempotent — deleting an absent path is `Ok`).
    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr>;
}

/// Lowercase hex SHA-256 of `content` — the CAS token (Anthropic wire is sha256).
#[must_use]
pub fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Validate a memory path: absolute, non-root, no `//`, no `.`/`..` segment, no
/// control chars, ≤ [`MAX_PATH_BYTES`].
fn validate_path(path: &str) -> Result<(), MemErr> {
    let bad = |p: &str| MemErr::InvalidPath(p.to_string());
    if path.len() > MAX_PATH_BYTES
        || !path.starts_with('/')
        || path == "/"
        || path.contains("//")
        || path.chars().any(|c| c.is_control())
    {
        return Err(bad(path));
    }
    for seg in path.split('/').skip(1) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(bad(path));
        }
    }
    Ok(())
}

fn validate_size(content: &str) -> Result<(), MemErr> {
    if content.len() > MAX_MEMORY_BYTES {
        Err(MemErr::TooLarge)
    } else {
        Ok(())
    }
}

/// The durable/in-memory record; also the on-disk JSON for [`FsMemoryFs`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    path: String,
    content: String,
    sha: String,
    version: u64,
    created: u128,
    updated: u128,
}

impl Record {
    fn to_memory(&self, with_content: bool) -> Memory {
        Memory {
            id: self.id.clone(),
            path: self.path.clone(),
            content_sha256: self.sha.clone(),
            content_size: self.content.len() as u64,
            version: self.version,
            created_unix_nanos: self.created,
            updated_unix_nanos: self.updated,
            content: with_content.then(|| self.content.clone()),
        }
    }
    fn to_entry(&self) -> MemoryEntry {
        MemoryEntry {
            id: self.id.clone(),
            path: self.path.clone(),
            content_sha256: self.sha.clone(),
            content_size: self.content.len() as u64,
            version: self.version,
            updated_unix_nanos: self.updated,
        }
    }
}

fn under_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        return true;
    }
    let p = prefix.trim_end_matches('/');
    path == p || path.starts_with(&format!("{p}/"))
}

// ---------------------------------------------------------------------------
// In-memory backend
// ---------------------------------------------------------------------------

/// In-memory [`MemoryFs`] (tests / ephemeral single-process). One lock guards the
/// whole map, so every create/update/rename/delete is atomic and CAS is race-free.
#[derive(Default)]
pub struct InMemoryFs {
    // store id → (path → record)
    inner: Mutex<BTreeMap<String, BTreeMap<String, Record>>>,
    next: AtomicU64,
}

impl InMemoryFs {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn mint_id(&self) -> String {
        format!("mem_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

#[async_trait]
impl MemoryFs for InMemoryFs {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .get(store)
            .map(|s| {
                s.values()
                    .filter(|r| under_prefix(&r.path, prefix))
                    .map(Record::to_entry)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .get(store)
            .and_then(|s| s.get(path))
            .map(|r| r.to_memory(true)))
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let id = self.mint_id();
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard.entry(store.to_string()).or_default();
        if store_map.contains_key(path) {
            return Err(MemErr::PathConflict(path.to_string()));
        }
        let now = now_nanos();
        let record = Record {
            id,
            path: path.to_string(),
            sha: sha256_hex(content),
            content: content.to_string(),
            version: 1,
            created: now,
            updated: now,
        };
        let memory = record.to_memory(true);
        store_map.insert(path.to_string(), record);
        Ok(memory)
    }

    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        let new_sha = sha256_hex(content);
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard
            .get_mut(store)
            .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        let record = store_map
            .values_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        if record.sha != base_sha {
            // Idempotent: the caller's write already matches the live content.
            if record.sha == new_sha {
                return Ok(record.to_memory(true));
            }
            return Err(MemErr::Conflict {
                current: Box::new(record.to_memory(true)),
            });
        }
        record.content = content.to_string();
        record.sha = new_sha;
        record.version += 1;
        record.updated = now_nanos();
        Ok(record.to_memory(true))
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard
            .get_mut(store)
            .ok_or_else(|| MemErr::NotFound(from.to_string()))?;
        if from == to {
            return store_map
                .get(from)
                .map(|r| r.to_memory(true))
                .ok_or_else(|| MemErr::NotFound(from.to_string()));
        }
        let mut record = store_map
            .remove(from)
            .ok_or_else(|| MemErr::NotFound(from.to_string()))?;
        // Atomic replace of the destination (POSIX rename): the id/version below are
        // the moved memory's; any prior `to` is dropped.
        record.path = to.to_string();
        record.version += 1;
        record.updated = now_nanos();
        let memory = record.to_memory(true);
        store_map.insert(to.to_string(), record);
        Ok(memory)
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(s) = guard.get_mut(store) {
            s.remove(path);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

/// Durable [`MemoryFs`]: `<root>/<store>/<hex(sha256(path))>.mem`, one JSON record
/// per memory, written atomically (temp + rename). A single async mutex serializes
/// mutations so an in-process read-modify-write (CAS, rename-replace) is atomic; a
/// cross-node CAS is the postgres slice (ADR-0053 P4).
pub struct FsMemoryFs {
    root: PathBuf,
    next: AtomicU64,
    write_lock: tokio::sync::Mutex<()>,
}

impl FsMemoryFs {
    /// Open (creating if absent) the store rooted at `root`, seeding the id counter
    /// past the highest `mem_<n>` already persisted.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let mut max = 0u64;
        if let Ok(stores) = std::fs::read_dir(&root) {
            for store in stores.flatten() {
                if let Ok(files) = std::fs::read_dir(store.path()) {
                    for entry in files.flatten() {
                        if let Ok(bytes) = std::fs::read(entry.path())
                            && let Ok(rec) = serde_json::from_slice::<Record>(&bytes)
                            && let Some(n) =
                                rec.id.strip_prefix("mem_").and_then(|d| d.parse().ok())
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
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn store_dir(&self, store: &str) -> PathBuf {
        self.root.join(crate::sanitize_stem(store))
    }
    fn record_path(&self, store: &str, path: &str) -> PathBuf {
        self.store_dir(store)
            .join(format!("{}.mem", sha256_hex(path)))
    }
    fn mint_id(&self) -> String {
        format!("mem_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn read_record(&self, file: &Path) -> Result<Option<Record>, MemErr> {
        match tokio::fs::read(file).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| MemErr::Storage(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemErr::Storage(e.to_string())),
        }
    }

    async fn write_record(&self, store: &str, record: &Record) -> Result<(), MemErr> {
        let dir = self.store_dir(store);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| MemErr::Storage(e.to_string()))?;
        let bytes = serde_json::to_vec(record).map_err(|e| MemErr::Storage(e.to_string()))?;
        let final_path = self.record_path(store, &record.path);
        let tmp = dir.join(format!(
            ".tmp.{}.{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::SeqCst)
        ));
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| MemErr::Storage(e.to_string()))?;
        tokio::fs::rename(&tmp, &final_path)
            .await
            .map_err(|e| MemErr::Storage(e.to_string()))
    }

    async fn all_records(&self, store: &str) -> Result<Vec<Record>, MemErr> {
        let dir = self.store_dir(store);
        let mut out = Vec::new();
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(MemErr::Storage(e.to_string())),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| MemErr::Storage(e.to_string()))?
        {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("mem")
                && let Some(rec) = self.read_record(&p).await?
            {
                out.push(rec);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl MemoryFs for FsMemoryFs {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        Ok(self
            .all_records(store)
            .await?
            .iter()
            .filter(|r| under_prefix(&r.path, prefix))
            .map(Record::to_entry)
            .collect())
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        Ok(self
            .read_record(&self.record_path(store, path))
            .await?
            .map(|r| r.to_memory(true)))
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let _guard = self.write_lock.lock().await;
        if self.record_path(store, path).exists() {
            return Err(MemErr::PathConflict(path.to_string()));
        }
        let now = now_nanos();
        let record = Record {
            id: self.mint_id(),
            path: path.to_string(),
            sha: sha256_hex(content),
            content: content.to_string(),
            version: 1,
            created: now,
            updated: now,
        };
        self.write_record(store, &record).await?;
        Ok(record.to_memory(true))
    }

    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        let new_sha = sha256_hex(content);
        let _guard = self.write_lock.lock().await;
        let mut record = self
            .all_records(store)
            .await?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        if record.sha != base_sha {
            if record.sha == new_sha {
                return Ok(record.to_memory(true));
            }
            return Err(MemErr::Conflict {
                current: Box::new(record.to_memory(true)),
            });
        }
        record.content = content.to_string();
        record.sha = new_sha;
        record.version += 1;
        record.updated = now_nanos();
        self.write_record(store, &record).await?;
        Ok(record.to_memory(true))
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let _guard = self.write_lock.lock().await;
        let from_file = self.record_path(store, from);
        let mut record = self
            .read_record(&from_file)
            .await?
            .ok_or_else(|| MemErr::NotFound(from.to_string()))?;
        if from == to {
            return Ok(record.to_memory(true));
        }
        record.path = to.to_string();
        record.version += 1;
        record.updated = now_nanos();
        // Write the destination (replacing any prior `to`), then drop the source.
        self.write_record(store, &record).await?;
        let _ = tokio::fs::remove_file(&from_file).await;
        Ok(record.to_memory(true))
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        let _guard = self.write_lock.lock().await;
        match tokio::fs::remove_file(self.record_path(store, path)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MemErr::Storage(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "awaken-memfs-{tag}-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        std::fs::remove_dir_all(&p).ok();
        p
    }

    /// Run the same conformance suite over any `MemoryFs` backend.
    async fn conformance(fs: &dyn MemoryFs) {
        let store = "memstore_1";

        // create → version 1, sha set, content round-trips.
        let m = fs.create(store, "/notes/today.md", "alpha").await.unwrap();
        assert_eq!(m.path, "/notes/today.md");
        assert_eq!(m.version, 1);
        assert_eq!(m.content_sha256, sha256_hex("alpha"));
        assert_eq!(m.content.as_deref(), Some("alpha"));
        assert!(m.created_unix_nanos > 0 && m.updated_unix_nanos >= m.created_unix_nanos);

        // duplicate path → PathConflict.
        assert!(matches!(
            fs.create(store, "/notes/today.md", "x").await,
            Err(MemErr::PathConflict(_))
        ));

        // get_by_path.
        let got = fs
            .get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.content.as_deref(), Some("alpha"));
        assert!(fs.get_by_path(store, "/nope.md").await.unwrap().is_none());

        // update with the right base sha → version bumps, content changes.
        let up = fs
            .update(store, &m.id, "beta", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(up.version, 2);
        assert_eq!(up.content.as_deref(), Some("beta"));

        // update with a STALE base sha (the original) → Conflict carrying live "beta".
        match fs.update(store, &m.id, "gamma", &m.content_sha256).await {
            Err(MemErr::Conflict { current }) => {
                assert_eq!(current.content.as_deref(), Some("beta"));
                assert_eq!(current.content_sha256, sha256_hex("beta"));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        // ...and the store still holds "beta" (no clobber).
        assert_eq!(
            fs.get_by_path(store, "/notes/today.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("beta")
        );

        // idempotent update: stale base sha but content already current → Ok.
        let idem = fs.update(store, &m.id, "beta", "deadbeef").await.unwrap();
        assert_eq!(idem.content.as_deref(), Some("beta"));

        // list under a prefix.
        fs.create(store, "/notes/other.md", "o").await.unwrap();
        fs.create(store, "/root.md", "r").await.unwrap();
        let mut notes: Vec<_> = fs
            .list(store, "/notes")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        notes.sort();
        assert_eq!(notes, vec!["/notes/other.md", "/notes/today.md"]);
        assert_eq!(fs.list(store, "/").await.unwrap().len(), 3);

        // rename-replace: move over an existing target atomically, id preserved.
        let before = fs
            .get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap();
        let moved = fs
            .rename(store, "/notes/today.md", "/notes/other.md")
            .await
            .unwrap();
        assert_eq!(moved.id, before.id, "rename preserves the memory id");
        assert_eq!(moved.path, "/notes/other.md");
        assert_eq!(moved.content.as_deref(), Some("beta"));
        assert!(
            fs.get_by_path(store, "/notes/today.md")
                .await
                .unwrap()
                .is_none(),
            "the source path is gone after rename"
        );
        assert_eq!(
            fs.get_by_path(store, "/notes/other.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("beta"),
            "the destination was atomically replaced with the source content"
        );

        // delete (idempotent).
        fs.delete_by_path(store, "/notes/other.md").await.unwrap();
        assert!(
            fs.get_by_path(store, "/notes/other.md")
                .await
                .unwrap()
                .is_none()
        );
        fs.delete_by_path(store, "/notes/other.md").await.unwrap(); // absent → Ok

        // path validation.
        assert!(matches!(
            fs.create(store, "relative.md", "x").await,
            Err(MemErr::InvalidPath(_))
        ));
        assert!(matches!(
            fs.create(store, "/a/../b.md", "x").await,
            Err(MemErr::InvalidPath(_))
        ));
        assert!(matches!(
            fs.create(store, "/a//b.md", "x").await,
            Err(MemErr::InvalidPath(_))
        ));

        // size cap.
        let big = "x".repeat(MAX_MEMORY_BYTES + 1);
        assert!(matches!(
            fs.create(store, "/big.md", &big).await,
            Err(MemErr::TooLarge)
        ));
    }

    #[tokio::test]
    async fn in_memory_backend_conforms() {
        conformance(&InMemoryFs::new()).await;
    }

    #[tokio::test]
    async fn fs_backend_conforms() {
        let root = temp_root("conf");
        let fs = FsMemoryFs::open(&root).unwrap();
        conformance(&fs).await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn fs_backend_survives_reopen() {
        let root = temp_root("reopen");
        {
            let fs = FsMemoryFs::open(&root).unwrap();
            fs.create("s1", "/keep.md", "durable").await.unwrap();
        }
        // A fresh handle over the same root sees the persisted memory and does not
        // re-mint its id.
        let fs = FsMemoryFs::open(&root).unwrap();
        let got = fs.get_by_path("s1", "/keep.md").await.unwrap().unwrap();
        assert_eq!(got.content.as_deref(), Some("durable"));
        let fresh = fs.create("s1", "/new.md", "n").await.unwrap();
        assert_ne!(fresh.id, got.id, "reopened store mints a fresh id");
        std::fs::remove_dir_all(&root).ok();
    }
}
