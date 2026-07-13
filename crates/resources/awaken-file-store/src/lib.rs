//! Content-addressed blob store (ADR-0041) — the neutral hub for sandbox mounts and
//! artifacts. `put(bytes) -> id` where **`id` is the BLAKE3 content hash**; `get(id)`
//! resolves it. The store is immutable and deduplicating: equal bytes always yield
//! the same id, on every backend, so mirroring/migration is "copy by id" and a
//! mount's declared `content_hash` verifies fail-closed.
//!
//! The trait is **async** so network/db backends (`awaken-file-store-postgres`,
//! `awaken-file-store-s3`) fit the same seam as the local ones here (`FsFileStore`,
//! `InMemoryFileStore`). The **id is computed in this core**, never in a backend, so
//! it is identical across every implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::Mutex;

// The `FileStore` port + its error live in the port-only contract crate; this crate
// implements them and re-exports so `awaken_file_store::FileStore` keeps resolving.
pub use awaken_resource_contract::{FileStore, FileStoreError};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// The content-addressed id of `bytes`: a BLAKE3 hex digest. Stable across process
/// runs, Rust versions, and every backend — unlike a `DefaultHasher` fingerprint.
#[must_use]
pub fn content_id(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Filesystem-backed store: one file per blob, named by its content id. `put` writes
/// to a temp file and atomically renames into place (crash-safe, idempotent).
pub struct FsFileStore {
    base: PathBuf,
}

impl FsFileStore {
    /// Open (creating the base directory) a store rooted at `base`.
    pub async fn open(base: impl Into<PathBuf>) -> Result<Self, FileStoreError> {
        let base = base.into();
        tokio::fs::create_dir_all(&base).await.map_err(e)?;
        Ok(Self { base })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.base.join(id)
    }
}

#[async_trait]
impl FileStore for FsFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        let path = self.path(&id);
        if tokio::fs::try_exists(&path).await.map_err(e)? {
            return Ok(id); // immutable + deduplicating: already present
        }
        // Atomic publish: write a unique temp file, then rename onto the id path.
        let tmp = self.base.join(format!(".tmp-{id}"));
        tokio::fs::write(&tmp, bytes).await.map_err(e)?;
        tokio::fs::rename(&tmp, &path).await.map_err(e)?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        match tokio::fs::read(self.path(id)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(e(err)),
        }
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let mut ids = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.base).await.map_err(e)?;
        while let Some(entry) = dir.next_entry().await.map_err(e)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                continue; // skip in-flight writes
            }
            if entry
                .file_type()
                .await
                .map(|t| t.is_file())
                .unwrap_or(false)
            {
                ids.push(name);
            }
        }
        ids.sort();
        Ok(ids)
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        match tokio::fs::remove_file(self.path(id)).await {
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FileStore for InMemoryFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        self.blobs.lock().await.insert(id.clone(), bytes.to_vec());
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        Ok(self.blobs.lock().await.get(id).cloned())
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let mut ids: Vec<String> = self.blobs.lock().await.keys().cloned().collect();
        ids.sort();
        Ok(ids)
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        Ok(self.blobs.lock().await.remove(id).is_some())
    }
}

/// A path helper for backends that stage into a directory (not part of the trait).
pub fn is_content_id(base: &Path, id: &str) -> bool {
    base.join(id).exists()
}

/// The portable schema shared by the durable relational backends (`postgres` /
/// `sqlite` features).
#[cfg(any(feature = "postgres", feature = "sqlite"))]
pub mod schema;

/// Postgres `bytea` backend (`postgres` feature).
#[cfg(feature = "postgres")]
pub mod postgres;

/// SQLite `BLOB` backend (`sqlite` feature).
#[cfg(feature = "sqlite")]
pub mod sqlite;

/// Object-store backend — S3/MinIO/GCS/Azure (`s3` feature).
#[cfg(feature = "s3")]
pub mod s3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_id_is_blake3_and_stable() {
        // Known BLAKE3 of the empty input (regression-guards the algorithm choice).
        assert_eq!(
            content_id(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(content_id(b"hello"), content_id(b"hello"));
        assert_ne!(content_id(b"hello"), content_id(b"world"));
    }

    async fn round_trip(store: &dyn FileStore) {
        let id = store.put(b"hello world").await.unwrap();
        assert_eq!(id, store.put(b"hello world").await.unwrap(), "idempotent");
        assert_ne!(id, store.put(b"different").await.unwrap());
        assert_eq!(
            store.get(&id).await.unwrap().as_deref(),
            Some(&b"hello world"[..])
        );
        assert!(store.get("nonexistent").await.unwrap().is_none());
        assert!(store.list().await.unwrap().contains(&id));
        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_round_trip() {
        round_trip(&InMemoryFileStore::new()).await;
    }

    #[tokio::test]
    async fn fs_round_trip_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsFileStore::open(tmp.path()).await.unwrap();
        round_trip(&store).await;

        let id = store.put(b"durable").await.unwrap();
        drop(store);
        let reopened = FsFileStore::open(tmp.path()).await.unwrap();
        assert_eq!(
            reopened.get(&id).await.unwrap().as_deref(),
            Some(&b"durable"[..])
        );
    }

    #[tokio::test]
    async fn same_bytes_same_id_across_backends() {
        let mem = InMemoryFileStore::new();
        let tmp = tempfile::tempdir().unwrap();
        let fs = FsFileStore::open(tmp.path()).await.unwrap();
        // The id is computed in the core, so it matches across implementations.
        assert_eq!(
            mem.put(b"portable").await.unwrap(),
            fs.put(b"portable").await.unwrap()
        );
    }

    /// Live Postgres round-trip for [`PgFileStore`](crate::postgres::PgFileStore),
    /// isolated in its own schema. Skips when no Postgres is reachable
    /// (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle renders and
    /// runs on Postgres too.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_round_trip() {
        use crate::postgres::PgFileStore;

        let base = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
        let Ok(admin) = sqlx::PgPool::connect(&base).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        use sqlx::Executor;
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_file_store CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_file_store")
            .await
            .expect("create schema");
        admin.close().await;
        let sep = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{sep}options=-c%20search_path%3Dt_file_store");
        let store = PgFileStore::connect(&url).await.unwrap();
        round_trip(&store).await;
        // Same id as the core, across the network backend too.
        assert_eq!(
            store.put(b"portable").await.unwrap(),
            InMemoryFileStore::new().put(b"portable").await.unwrap()
        );
    }

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;
        use crate::sqlite::SqliteFileStore;

        #[tokio::test]
        async fn sqlite_round_trip() {
            round_trip(&SqliteFileStore::open_in_memory().unwrap()).await;
        }

        #[tokio::test]
        async fn sqlite_persists_and_shares_the_core_id() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("blobs.db");
            let path = path.to_str().unwrap();
            let id = {
                let store = SqliteFileStore::open(path).unwrap();
                // Same id as the in-memory backend (computed in the core).
                assert_eq!(
                    store.put(b"portable").await.unwrap(),
                    InMemoryFileStore::new().put(b"portable").await.unwrap()
                );
                store.put(b"durable").await.unwrap()
            };
            // A fresh handle on the same file sees the blob (idempotent migration).
            let reopened = SqliteFileStore::open(path).unwrap();
            assert_eq!(
                reopened.get(&id).await.unwrap().as_deref(),
                Some(&b"durable"[..])
            );
        }
    }
}
