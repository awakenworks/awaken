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
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Whether `id` names exactly one file directly under the base — non-empty and made
/// only of `[A-Za-z0-9_-]`. Ids minted by [`content_id`] are BLAKE3 hex and always
/// pass, but `get`/`delete` take an id off the wire, so a crafted `../` or absolute id
/// must resolve to *no file* rather than escape the base. (`.` is not alphanumeric, so
/// `..` and any `/` are rejected here.)
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Filesystem-backed store: one file per blob, named by its content id. `put` writes
/// to a temp file and atomically renames into place (crash-safe, idempotent).
pub struct FsFileStore {
    base: PathBuf,
    // Per-call sequence so two concurrent `put`s of the *same* bytes stage into
    // distinct temp files and never race on a shared one.
    seq: AtomicU64,
}

impl FsFileStore {
    /// Open (creating the base directory) a store rooted at `base`.
    pub async fn open(base: impl Into<PathBuf>) -> Result<Self, FileStoreError> {
        let base = base.into();
        tokio::fs::create_dir_all(&base).await.map_err(e)?;
        Ok(Self {
            base,
            seq: AtomicU64::new(0),
        })
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
        // Atomic publish: write a per-call-unique temp file, then rename onto the id
        // path. The temp name carries pid + a local sequence so concurrent writers of
        // identical bytes never share (and race to rename) one temp file.
        let tmp = self.base.join(format!(
            ".tmp-{id}-{}-{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&tmp, bytes).await.map_err(e)?;
        tokio::fs::rename(&tmp, &path).await.map_err(e)?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        if !safe_id(id) {
            return Ok(None); // a crafted id resolves to nothing; it cannot escape base
        }
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
        if !safe_id(id) {
            return Ok(false); // a crafted id names no blob; never delete outside base
        }
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
        let listed = store.list().await.unwrap();
        assert!(listed.contains(&id));
        // Contract guarantee (uniform across backends): ids come back sorted ascending.
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted, "list is sorted ascending");
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

    /// G-F1: a crashed/in-flight write leaves a `.tmp-...` file in the base; `list`
    /// must report only committed blobs, never the temp.
    #[tokio::test]
    async fn fs_list_excludes_in_flight_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsFileStore::open(tmp.path()).await.unwrap();
        let id = store.put(b"real blob").await.unwrap();
        tokio::fs::write(tmp.path().join(".tmp-deadbeef-1-2"), b"partial")
            .await
            .unwrap();
        assert_eq!(store.list().await.unwrap(), vec![id]);
    }

    /// G-F2 (R1): `get`/`delete` take an id off the wire, so a crafted `../` id must
    /// resolve to nothing and can neither read nor delete a file outside the base.
    #[tokio::test]
    async fn fs_crafted_ids_cannot_escape_root() {
        let parent = tempfile::tempdir().unwrap();
        let secret = parent.path().join("secret.txt");
        tokio::fs::write(&secret, b"top secret").await.unwrap();
        let store = FsFileStore::open(parent.path().join("store"))
            .await
            .unwrap();
        assert!(store.get("../secret.txt").await.unwrap().is_none());
        assert!(!store.delete("../secret.txt").await.unwrap());
        assert!(
            secret.exists(),
            "the outside file was neither read nor deleted"
        );
    }

    /// G-F3 (R2): concurrent `put`s of identical bytes all agree on the one content id
    /// and all succeed — no writer errors on a temp-file race, exactly one blob lands.
    #[tokio::test]
    async fn fs_concurrent_identical_puts_all_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FsFileStore::open(tmp.path()).await.unwrap());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            handles.push(tokio::spawn(async move { store.put(b"same bytes").await }));
        }
        let id0 = content_id(b"same bytes");
        for h in handles {
            assert_eq!(
                h.await.unwrap().expect("identical put must not race-error"),
                id0
            );
        }
        assert_eq!(
            store.get(&id0).await.unwrap().as_deref(),
            Some(&b"same bytes"[..])
        );
        assert_eq!(store.list().await.unwrap(), vec![id0]);
    }

    /// The contract's ascending-sort guarantee, exercised with several distinct blobs
    /// inserted out of sorted-id order plus a duplicate: `list` must return the *exact*
    /// id set, strictly ascending, with no duplicate for the repeated bytes. Stronger
    /// than "the returned list happens to be sorted": it pins the set and the order.
    async fn list_sorted_and_dedup(store: &dyn FileStore) {
        let inputs: [&[u8]; 5] = [b"delta", b"alpha", b"charlie", b"bravo", b"echo"];
        let mut expected = Vec::new();
        for bytes in inputs {
            let id = store.put(bytes).await.unwrap();
            // idempotent dedup: the same bytes a second time add no second entry.
            assert_eq!(store.put(bytes).await.unwrap(), id, "put is idempotent");
            expected.push(id);
        }
        expected.sort();
        let listed = store.list().await.unwrap();
        assert_eq!(
            listed, expected,
            "list returns the exact id set, ascending, deduped"
        );
        assert!(
            listed.windows(2).all(|w| w[0] < w[1]),
            "strictly ascending, no duplicate entries"
        );
    }

    /// Empty bytes are a legitimate blob: they hash to a stable id and round-trip like
    /// any other (the fs backend writes and renames a zero-length file).
    async fn empty_bytes_round_trip(store: &dyn FileStore) {
        let id = store.put(b"").await.unwrap();
        assert_eq!(id, content_id(b""));
        assert_eq!(store.get(&id).await.unwrap().as_deref(), Some(&b""[..]));
        assert!(store.list().await.unwrap().contains(&id));
        assert!(store.delete(&id).await.unwrap());
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_list_sorted_and_dedup() {
        list_sorted_and_dedup(&InMemoryFileStore::new()).await;
    }

    #[tokio::test]
    async fn fs_list_sorted_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        list_sorted_and_dedup(&FsFileStore::open(tmp.path()).await.unwrap()).await;
    }

    #[tokio::test]
    async fn in_memory_empty_bytes_round_trip() {
        empty_bytes_round_trip(&InMemoryFileStore::new()).await;
    }

    #[tokio::test]
    async fn fs_empty_bytes_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        empty_bytes_round_trip(&FsFileStore::open(tmp.path()).await.unwrap()).await;
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
        async fn sqlite_list_sorted_and_dedup() {
            list_sorted_and_dedup(&SqliteFileStore::open_in_memory().unwrap()).await;
        }

        #[tokio::test]
        async fn sqlite_empty_bytes_round_trip() {
            empty_bytes_round_trip(&SqliteFileStore::open_in_memory().unwrap()).await;
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
