//! Content-addressed blob store (ADR-0041) — the neutral hub for sandbox mounts and
//! artifacts. `put(bytes) -> id` where **`id` is the BLAKE3 content hash**; `get(id)`
//! resolves it. The store is immutable and deduplicating: equal bytes always yield
//! the same id, on every backend, so mirroring/migration is "copy by id" and a
//! mount's declared `content_hash` verifies fail-closed.
//!
//! The trait is **async** so network/db backends (`awaken-file-store-postgres`,
//! `awaken-file-store-s3`) fit the same seam as the local ones here (`FsFileStore`,
//! `InMemoryFileStore`). The **id is computed in this core**, never in a backend, so
//! it is identical across every implementation. `InMemoryFileStore` is available
//! only to tests or the explicit `test-support` feature.
//!
//! [`FileCatalog`] is implemented by the same durable adapters and owns public
//! Files API identity/metadata. Its opaque `file_...` ids are deliberately not
//! the content ids described above: equal bytes deduplicate without merging two
//! logical Files or their lifecycles.

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
#[cfg(any(test, feature = "test-support"))]
use tokio::sync::Mutex;

// The `FileStore` port + its error live in the port-only contract crate; this crate
// implements them and re-exports so `awaken_file_store::FileStore` keeps resolving.
pub use awaken_resource_contract::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, FileStore, FileStoreError,
    content_id,
};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// Stable, database-portable identity for one Sandbox artifact harvest.
/// Length framing prevents tuple ambiguity; the digest keeps internal tuple
/// components and PostgreSQL-forbidden NUL separators out of persistence.
#[must_use]
pub fn harvest_idempotency_key(thread: &str, logical_path: &str, content_id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"awaken-file-harvest-v1\0");
    for component in [thread, logical_path, content_id] {
        hash.update(&(component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    hash.finalize().to_hex().to_string()
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

/// In-memory store for unit, integration, and scenario fixtures.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryFileStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    files: Mutex<HashMap<String, FileRecord>>,
}

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
fn validate_record(record: &FileRecord) -> Result<(), FileCatalogError> {
    if record.id.trim().is_empty()
        || record.workspace_id.trim().is_empty()
        || record.blob_id.trim().is_empty()
        || record.filename.is_empty()
        || record.mime_type.trim().is_empty()
        || record.created_at.trim().is_empty()
    {
        return Err(FileCatalogError::Invalid(
            "id, workspace, blob, filename, MIME type, and created_at are required".into(),
        ));
    }
    Ok(())
}

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
impl FileCatalog for InMemoryFileStore {
    async fn create_file(
        &self,
        record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        validate_record(&record)?;
        let mut files = self.files.lock().await;
        if let Some(key) = record.harvest_key.as_deref()
            && let Some(existing) = files.values().find(|candidate| {
                !candidate.deleted
                    && candidate.workspace_id == record.workspace_id
                    && candidate.harvest_key.as_deref() == Some(key)
            })
        {
            return Ok(CreateFileRecordOutcome::Existing(existing.clone()));
        }
        if files.contains_key(&record.id) {
            return Err(FileCatalogError::Invalid(format!(
                "file id `{}` already exists",
                record.id
            )));
        }
        files.insert(record.id.clone(), record.clone());
        Ok(CreateFileRecordOutcome::Inserted(record))
    }

    async fn get_file(
        &self,
        workspace_id: &str,
        file_id: &str,
        include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(self
            .files
            .lock()
            .await
            .get(file_id)
            .filter(|record| {
                record.workspace_id == workspace_id && (include_deleted || !record.deleted)
            })
            .cloned())
    }

    async fn list_files(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        let mut records = self
            .files
            .lock()
            .await
            .values()
            .filter(|record| {
                !record.deleted
                    && record.workspace_id == workspace_id
                    && scope_id.is_none_or(|scope| record.scope_id.as_deref() == Some(scope))
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(records)
    }

    async fn mark_file_deleted(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        let mut files = self.files.lock().await;
        let Some(record) = files
            .get_mut(file_id)
            .filter(|record| record.workspace_id == workspace_id)
        else {
            return Ok(None);
        };
        record.deleted = true;
        Ok(Some(record.clone()))
    }

    async fn active_size_bytes(&self, workspace_id: &str) -> Result<u64, FileCatalogError> {
        Ok(self
            .files
            .lock()
            .await
            .values()
            .filter(|record| !record.deleted && record.workspace_id == workspace_id)
            .map(|record| record.size_bytes)
            .sum())
    }
}

#[cfg(any(test, feature = "test-support"))]
impl InMemoryFileStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
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

    fn file_record(
        id: &str,
        workspace: &str,
        created_at: &str,
        scope: Option<&str>,
        harvest_key: Option<&str>,
    ) -> FileRecord {
        FileRecord {
            id: id.into(),
            workspace_id: workspace.into(),
            blob_id: format!("blob-{id}"),
            filename: format!("{id}.txt"),
            mime_type: "text/plain".into(),
            size_bytes: 3,
            created_at: created_at.into(),
            downloadable: scope.is_some(),
            scope_id: scope.map(str::to_string),
            logical_path: scope.map(|_| format!("{id}.txt")),
            harvest_key: harvest_key.map(str::to_string),
            deleted: false,
        }
    }

    async fn catalog_contract(store: &dyn FileCatalog) {
        // Cause/effect graph:
        // C1 unique upload; C2 same harvest key; C3 Workspace/scope selector;
        // C4 logical delete. Effects: E1 insert, E2 recover original idempotently,
        // E3 isolation + newest-first order, E4 active-byte decrement and hidden read.
        // Decision rules R1..R4 are exercised in order below for every backend.
        let old = file_record("file_old", "w1", "2026-01-01T00:00:00Z", None, None);
        let harvest_key = harvest_idempotency_key("session-1", "out.txt", "hash");
        let scoped = file_record(
            "file_scoped",
            "w1",
            "2026-01-02T00:00:00Z",
            Some("session-1"),
            Some(&harvest_key),
        );
        let other_workspace = file_record(
            "file_other",
            "w2",
            "2026-01-03T00:00:00Z",
            Some("session-1"),
            Some(&harvest_key),
        );
        assert!(matches!(
            store.create_file(old.clone()).await.unwrap(),
            CreateFileRecordOutcome::Inserted(_)
        ));
        store.create_file(scoped.clone()).await.unwrap();
        store.create_file(other_workspace).await.unwrap();

        let retry = FileRecord {
            id: "file_retry_candidate".into(),
            ..scoped.clone()
        };
        assert_eq!(
            store.create_file(retry).await.unwrap().record().id,
            scoped.id,
            "R2: harvest retry returns the committed logical File"
        );
        assert_eq!(
            store
                .list_files("w1", None)
                .await
                .unwrap()
                .into_iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![scoped.id.clone(), old.id.clone()]
        );
        assert_eq!(
            store.list_files("w1", Some("session-1")).await.unwrap(),
            vec![scoped.clone()]
        );
        assert!(
            store
                .list_files("w1", Some("session-2"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.list_files("w2", Some("session-1")).await.unwrap()[0].id,
            "file_other",
            "harvest idempotency is Workspace-scoped"
        );
        assert_eq!(store.active_size_bytes("w1").await.unwrap(), 6);

        let tombstone = store
            .mark_file_deleted("w1", &scoped.id)
            .await
            .unwrap()
            .unwrap();
        assert!(tombstone.deleted);
        assert!(
            store
                .get_file("w1", &scoped.id, false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_file("w1", &scoped.id, true)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.active_size_bytes("w1").await.unwrap(), 3);
        let replacement = FileRecord {
            id: "file_reharvested".into(),
            ..scoped
        };
        assert!(matches!(
            store.create_file(replacement).await.unwrap(),
            CreateFileRecordOutcome::Inserted(_)
        ));
    }

    #[test]
    fn harvest_key_is_framed_portable_and_sensitive_to_every_component() {
        // Cause/effect decision table: C1=the same ordered tuple; C2=one tuple
        // component changes; C3=components contain delimiter-like text. R1 C1
        // -> the same key; R2 C2 -> a different key; R3 C3 -> printable,
        // NUL-free key. Length framing makes R3 independent of delimiters.
        let key = harvest_idempotency_key("thread", "a\0b", "content");
        assert_eq!(key, harvest_idempotency_key("thread", "a\0b", "content"));
        assert_ne!(key, harvest_idempotency_key("thread-2", "a\0b", "content"));
        assert_ne!(key, harvest_idempotency_key("thread", "a\0b-2", "content"));
        assert_ne!(key, harvest_idempotency_key("thread", "a\0b", "content-2"));
        assert!(!key.contains('\0'));
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
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
    async fn in_memory_catalog_obeys_identity_scope_idempotency_and_delete_contract() {
        catalog_contract(&InMemoryFileStore::new()).await;
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

        // Causal graph: verify -> read ledger -> serve/fail; only migrate may
        // create ledger/tables. The table pins all three startup decisions:
        // | ledger | operation | result  | schema write |
        // | absent | verify    | failure | none         |
        // | absent | migrate   | success | apply bundle |
        // | current| verify    | success | none         |
        assert!(PgFileStore::connect_existing(&url).await.is_err());
        let verification_pool = sqlx::PgPool::connect(&url).await.unwrap();
        let ledger_after_verify: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('file_store_schema_migrations')::text")
                .fetch_one(&verification_pool)
                .await
                .unwrap();
        assert_eq!(ledger_after_verify, None, "verify never creates its ledger");
        verification_pool.close().await;

        let store = PgFileStore::connect(&url).await.unwrap();
        PgFileStore::connect_existing(&url).await.unwrap();
        round_trip(&store).await;
        catalog_contract(&store).await;
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
        async fn sqlite_catalog_obeys_identity_scope_idempotency_and_delete_contract() {
            catalog_contract(&SqliteFileStore::open_in_memory().unwrap()).await;
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
