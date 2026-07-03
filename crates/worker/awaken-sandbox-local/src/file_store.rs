//! Content-addressed blob store for `File`/`Resource` mount sources (ADR-0041
//! Slice 4). A provider resolves a mount's bytes from a [`FileStore`] and realizes
//! (fans out) a copy into the sandbox; the id is the content fingerprint, so a
//! store is immutable and deduplicating, and a mount's declared `content_hash` can
//! be verified fail-closed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::content_fingerprint;

/// A blob store failure.
#[derive(Debug, thiserror::Error)]
#[error("file store error: {0}")]
pub struct FileStoreError(pub String);

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// A content-addressed, immutable blob store. `put` returns the content id; `get`
/// resolves it. Backends: [`FsFileStore`] (durable) and [`InMemoryFileStore`].
pub trait FileStore: Send + Sync {
    /// Store `bytes`, returning the content-addressed id (idempotent for equal bytes).
    fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError>;
    /// Fetch by id, `None` if absent.
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError>;
    /// List all ids.
    fn list(&self) -> Result<Vec<String>, FileStoreError>;
    /// Delete by id; returns whether it existed.
    fn delete(&self, id: &str) -> Result<bool, FileStoreError>;
}

/// Filesystem-backed store: one file per blob, named by its content id.
pub struct FsFileStore {
    base: PathBuf,
}

impl FsFileStore {
    /// Open (creating the base directory) a store rooted at `base`.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self, FileStoreError> {
        let base = base.into();
        std::fs::create_dir_all(&base).map_err(e)?;
        Ok(Self { base })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.base.join(id)
    }
}

impl FileStore for FsFileStore {
    fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_fingerprint(bytes);
        let path = self.path(&id);
        if !path.exists() {
            std::fs::write(&path, bytes).map_err(e)?;
        }
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        match std::fs::read(self.path(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(e(err)),
        }
    }

    fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.base).map_err(e)?.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(e(err)),
        }
    }
}

/// In-memory store (tests, ephemeral runs).
#[derive(Default)]
pub struct InMemoryFileStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryFileStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl FileStore for InMemoryFileStore {
    fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_fingerprint(bytes);
        self.blobs
            .lock()
            .unwrap()
            .insert(id.clone(), bytes.to_vec());
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        Ok(self.blobs.lock().unwrap().get(id).cloned())
    }

    fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let mut ids: Vec<String> = self.blobs.lock().unwrap().keys().cloned().collect();
        ids.sort();
        Ok(ids)
    }

    fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        Ok(self.blobs.lock().unwrap().remove(id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(store: &dyn FileStore) {
        let id = store.put(b"hello world").unwrap();
        // content-addressed: equal bytes → equal id (dedup)
        assert_eq!(id, store.put(b"hello world").unwrap());
        assert_ne!(id, store.put(b"different").unwrap());

        assert_eq!(
            store.get(&id).unwrap().as_deref(),
            Some(&b"hello world"[..])
        );
        assert!(store.get("nonexistent").unwrap().is_none());

        assert!(store.list().unwrap().contains(&id));
        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap());
        assert!(store.get(&id).unwrap().is_none());
    }

    #[test]
    fn in_memory_round_trip() {
        round_trip(&InMemoryFileStore::new());
    }

    #[test]
    fn fs_round_trip_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsFileStore::open(tmp.path()).unwrap();
        round_trip(&store);

        // A blob survives re-opening the same base (durable).
        let id = store.put(b"durable").unwrap();
        drop(store);
        let reopened = FsFileStore::open(tmp.path()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().as_deref(), Some(&b"durable"[..]));
    }
}
