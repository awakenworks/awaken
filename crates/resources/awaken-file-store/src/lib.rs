//! Content-addressed blob store (ADR-0041) — the neutral hub for sandbox mounts and
//! artifacts. `put(bytes) -> id` where **`id` is the BLAKE3 content hash**; `get(id)`
//! resolves it. The store is immutable and deduplicating: equal bytes always yield
//! the same id, on every backend, so mirroring/migration is "copy by id" and a
//! mount's declared `content_hash` verifies fail-closed.
//!
//! The trait is **async** so network/db backends (`awaken-file-store-postgres`,
//! object storage) fit the same seam as the local ones here (`FsFileStore`,
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
use tokio::io::AsyncWriteExt;
#[cfg(any(test, feature = "test-support"))]
use tokio::sync::Mutex;

// The `FileStore` port + its error live in the port-only contract crate; this crate
// implements them and re-exports so `awaken_file_store::FileStore` keeps resolving.
#[cfg(test)]
use awaken_resource_contract::harvest_idempotency_key;
pub use awaken_resource_contract::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, FileStore, FileStoreError,
    content_id,
};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

static FILE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether `id` names exactly one file directly under the base — non-empty and made
/// only of `[A-Za-z0-9_-]`. Ids minted by [`content_id`] are BLAKE3 hex and always
/// pass, but `get`/`delete` take an id off the wire, so a crafted `../` or absolute id
/// must resolve to *no file* rather than escape the base. (`.` is not alphanumeric, so
/// `..` and any `/` are rejected here.)
pub(crate) fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
        sync_directory(&base).await?;
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
            let existing = tokio::fs::read(&path).await.map_err(e)?;
            if existing != bytes || content_id(&existing) != id {
                return Err(e(format!("corrupt content-addressed blob `{id}`")));
            }
            return Ok(id); // immutable + deduplicating: already present
        }
        // Atomic publish: write a per-process-unique temp file, then rename onto
        // the id path. PID + the process-global sequence also separates multiple
        // store instances pointed at the same root.
        let tmp = self.base.join(format!(
            ".tmp-{id}-{}-{}",
            std::process::id(),
            FILE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let publish = async {
            let mut temp = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .await
                .map_err(e)?;
            temp.write_all(bytes).await.map_err(e)?;
            temp.sync_all().await.map_err(e)?;
            drop(temp);
            tokio::fs::rename(&tmp, &path).await.map_err(e)
        }
        .await;
        if let Err(error) = publish {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        sync_directory(&self.base).await?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        if !safe_id(id) {
            return Ok(None); // a crafted id resolves to nothing; it cannot escape base
        }
        match tokio::fs::read(self.path(id)).await {
            Ok(bytes) if content_id(&bytes) == id => Ok(Some(bytes)),
            Ok(_) => Err(e(format!("corrupt content-addressed blob `{id}`"))),
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
            Ok(()) => {
                sync_directory(&self.base).await?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(e(err)),
        }
    }
}

async fn sync_directory(path: &Path) -> Result<(), FileStoreError> {
    tokio::fs::File::open(path)
        .await
        .map_err(e)?
        .sync_all()
        .await
        .map_err(e)
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
    if record
        .artifact_idempotency_scope
        .as_deref()
        .is_some_and(|scope| scope.trim().is_empty())
    {
        return Err(FileCatalogError::Invalid(
            "artifact idempotency scope must be nonempty".into(),
        ));
    }
    if record.artifact_idempotency_scope.is_some()
        && (record
            .harvest_key
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            || record
                .scope_id
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            || record
                .logical_path
                .as_deref()
                .is_none_or(|value| value.trim().is_empty()))
    {
        return Err(FileCatalogError::Invalid(
            "terminal artifact association requires harvest, Session, and logical-path identity"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
#[derive(Debug)]
enum ArtifactAssociationDecision {
    Existing(FileRecord),
    Associate(FileRecord),
}

/// Decide the one legal terminal association against every durable row for the
/// same Workspace/harvest identity. Active rows are the current logical File;
/// a single tombstone is recoverable only when no active row exists. Exact
/// scope replay wins across later ordinary re-harvests, while ambiguous or
/// foreign evidence fails closed.
#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
fn artifact_association_decision(
    records: &[FileRecord],
    candidate: &FileRecord,
) -> Result<Option<ArtifactAssociationDecision>, FileCatalogError> {
    let requested_scope = candidate
        .artifact_idempotency_scope
        .as_deref()
        .ok_or_else(|| {
            FileCatalogError::Invalid(
                "artifact association requires a terminal idempotency scope".into(),
            )
        })?;
    if records
        .iter()
        .any(|record| !same_harvest_identity(record, candidate))
    {
        return Err(FileCatalogError::Invalid(
            "artifact harvest key is bound to different File identity".into(),
        ));
    }
    let exact = records
        .iter()
        .filter(|record| record.artifact_idempotency_scope.as_deref() == Some(requested_scope))
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [] => {}
        [record] => {
            return Ok(Some(ArtifactAssociationDecision::Existing(
                (*record).clone(),
            )));
        }
        _ => {
            return Err(FileCatalogError::Invalid(
                "artifact File has duplicate terminal-operation evidence".into(),
            ));
        }
    }

    let active = records
        .iter()
        .filter(|record| !record.deleted)
        .collect::<Vec<_>>();
    let target = match active.as_slice() {
        [] => match records {
            [] => return Ok(None),
            [record] => record,
            _ => {
                return Err(FileCatalogError::Invalid(
                    "artifact File has ambiguous tombstoned harvest evidence".into(),
                ));
            }
        },
        [record] => *record,
        _ => {
            return Err(FileCatalogError::Invalid(
                "artifact File has duplicate active harvest evidence".into(),
            ));
        }
    };
    match target.artifact_idempotency_scope.as_deref() {
        None => Ok(Some(ArtifactAssociationDecision::Associate(target.clone()))),
        Some(_) => Err(FileCatalogError::Invalid(
            "artifact File is already associated with another terminal operation".into(),
        )),
    }
}

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
fn same_harvest_identity(left: &FileRecord, right: &FileRecord) -> bool {
    left.workspace_id == right.workspace_id
        && left.blob_id == right.blob_id
        && left.scope_id == right.scope_id
        && left.logical_path == right.logical_path
        && left.harvest_key == right.harvest_key
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
        if let Some(key) = record.harvest_key.as_deref() {
            let matching = files
                .values()
                .filter(|candidate| {
                    candidate.workspace_id == record.workspace_id
                        && candidate.harvest_key.as_deref() == Some(key)
                })
                .cloned()
                .collect::<Vec<_>>();
            if let Some(requested) = record.artifact_idempotency_scope.as_deref() {
                match artifact_association_decision(&matching, &record)? {
                    Some(ArtifactAssociationDecision::Existing(existing)) => {
                        return Ok(CreateFileRecordOutcome::Existing(existing));
                    }
                    Some(ArtifactAssociationDecision::Associate(target)) => {
                        let existing = files.get_mut(&target.id).ok_or_else(|| {
                            FileCatalogError::Storage(
                                "artifact File disappeared during association".into(),
                            )
                        })?;
                        existing.artifact_idempotency_scope = Some(requested.to_string());
                        return Ok(CreateFileRecordOutcome::Existing(existing.clone()));
                    }
                    None if record.deleted => {
                        return Err(FileCatalogError::Invalid(
                            "terminal association cannot recreate a missing tombstone".into(),
                        ));
                    }
                    None => {}
                }
            } else if let Some(existing) = matching.into_iter().find(|record| !record.deleted) {
                if !same_harvest_identity(&existing, &record) {
                    return Err(FileCatalogError::Invalid(
                        "artifact harvest key is bound to different File identity".into(),
                    ));
                }
                return Ok(CreateFileRecordOutcome::Existing(existing));
            }
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

    async fn list_files_including_deleted(
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
                record.workspace_id == workspace_id
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

/// Object-store backend — S3-compatible or GCS (`object-store` feature).
#[cfg(feature = "object-store")]
pub mod object;

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
        // Test design — backend-neutral immutable-blob contract:
        // Missing --put(bytes)--> Present(hash,bytes); identical put is a no-op,
        // distinct content has a distinct identity, list is deterministic, and
        // delete is idempotent. Every adapter runs this exact transition suite.
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
            expires_at: None,
            downloadable: scope.is_some(),
            scope_id: scope.map(str::to_string),
            logical_path: scope.map(|_| format!("{id}.txt")),
            harvest_key: harvest_key.map(str::to_string),
            artifact_idempotency_scope: None,
            deleted: false,
        }
    }

    async fn catalog_contract(store: &dyn FileCatalog) {
        // Cause/effect graph:
        // C1 unique upload; C2 same harvest key; C3 Workspace/scope selector;
        // C4 terminal scope None/same/foreign; C5 logical delete; C6 terminal
        // association after an ordinary row was tombstoned; C7 same key but a
        // different Session/path/content identity. Effects: E1
        // insert; E2 recover original idempotently; E3 isolation + newest-first
        // order; E4 atomically upgrade the same row None->Some, replay exact,
        // reject foreign; E5 active-byte decrement + hidden public read while
        // recovery evidence remains listable; E6 associate the tombstone in
        // place without resurrecting or duplicating it; E7 reject before any
        // association mutation. Rules R1..R7 run for every backend.
        let old = file_record("file_old", "w1", "2026-01-01T00:00:00Z", None, None);
        assert!(
            store
                .create_file(FileRecord {
                    id: "file_unbound_terminal_scope".into(),
                    artifact_idempotency_scope: Some("cleanup-unbound".into()),
                    ..old.clone()
                })
                .await
                .is_err(),
            "R7 terminal scope requires canonical artifact identity"
        );
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

        let tombstone_key = harvest_idempotency_key("session-1", "old.txt", "old-hash");
        let tombstone_before_terminal = file_record(
            "file_tombstone_before_terminal",
            "w1",
            "2026-01-01T12:00:00Z",
            Some("session-1"),
            Some(&tombstone_key),
        );
        store
            .create_file(tombstone_before_terminal.clone())
            .await
            .unwrap();
        store
            .mark_file_deleted("w1", &tombstone_before_terminal.id)
            .await
            .unwrap();
        let associated_tombstone = store
            .create_file(FileRecord {
                artifact_idempotency_scope: Some("cleanup-tombstone".into()),
                ..tombstone_before_terminal.clone()
            })
            .await
            .unwrap();
        assert_eq!(
            associated_tombstone.record().id,
            tombstone_before_terminal.id,
            "R6 preserves the logical File identity"
        );
        assert!(
            associated_tombstone.record().deleted,
            "R6 keeps the row tombstoned"
        );
        assert!(
            store
                .list_files("w1", Some("session-1"))
                .await
                .unwrap()
                .into_iter()
                .all(|record| record.harvest_key.as_deref() != Some(tombstone_key.as_str())),
            "R6 creates no active replacement"
        );
        assert_eq!(
            store
                .list_files_including_deleted("w1", Some("session-1"))
                .await
                .unwrap()
                .into_iter()
                .filter(|record| record.harvest_key.as_deref() == Some(tombstone_key.as_str()))
                .count(),
            1,
            "R6 leaves one durable row"
        );

        let terminal_candidate = FileRecord {
            id: "file_terminal_candidate".into(),
            artifact_idempotency_scope: Some("cleanup-a".into()),
            ..scoped.clone()
        };
        let associated = store.create_file(terminal_candidate.clone()).await.unwrap();
        assert_eq!(associated.record().id, scoped.id, "R4 same File identity");
        assert_eq!(
            associated.record().artifact_idempotency_scope.as_deref(),
            Some("cleanup-a"),
            "R4 None->Some"
        );
        assert_eq!(
            store
                .create_file(FileRecord {
                    id: "file_terminal_replay".into(),
                    ..terminal_candidate
                })
                .await
                .unwrap()
                .record()
                .id,
            scoped.id,
            "R4 exact replay"
        );
        assert!(
            store
                .create_file(FileRecord {
                    id: "file_terminal_foreign".into(),
                    artifact_idempotency_scope: Some("cleanup-b".into()),
                    ..scoped.clone()
                })
                .await
                .is_err(),
            "R4 foreign scope conflict"
        );
        assert!(
            store
                .create_file(FileRecord {
                    id: "file_terminal_substitution".into(),
                    blob_id: "foreign-content".into(),
                    artifact_idempotency_scope: Some("cleanup-a".into()),
                    ..scoped.clone()
                })
                .await
                .is_err(),
            "R7 a harvest key cannot substitute content identity"
        );

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
        let durable = store
            .list_files_including_deleted("w1", Some("session-1"))
            .await
            .unwrap()
            .into_iter()
            .filter(|record| record.harvest_key.as_deref() == Some(harvest_key.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(durable.len(), 1, "R5 recovery sees tombstone");
        assert!(durable[0].deleted, "R5");
        assert_eq!(
            durable[0].artifact_idempotency_scope.as_deref(),
            Some("cleanup-a"),
            "R5"
        );
        assert_eq!(store.active_size_bytes("w1").await.unwrap(), 3);
        let replacement = FileRecord {
            id: "file_reharvested".into(),
            ..scoped
        };
        assert!(matches!(
            store.create_file(replacement.clone()).await.unwrap(),
            CreateFileRecordOutcome::Inserted(_)
        ));
        assert_eq!(
            store
                .create_file(FileRecord {
                    id: "file_terminal_after_reharvest".into(),
                    artifact_idempotency_scope: Some("cleanup-a".into()),
                    ..replacement
                })
                .await
                .unwrap()
                .record()
                .id,
            tombstone.id,
            "R5 response-loss replay keeps the originally associated receipt even after re-harvest"
        );
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

    /// G-F3 (R2): concurrent `put`s through independent store instances all agree
    /// on one content id and succeed — no temp-file ownership race, one blob lands.
    #[tokio::test]
    async fn fs_concurrent_identical_puts_all_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let first = std::sync::Arc::new(FsFileStore::open(tmp.path()).await.unwrap());
        let second = std::sync::Arc::new(FsFileStore::open(tmp.path()).await.unwrap());
        let mut handles = Vec::new();
        for ordinal in 0..16 {
            let store = if ordinal % 2 == 0 {
                std::sync::Arc::clone(&first)
            } else {
                std::sync::Arc::clone(&second)
            };
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
            first.get(&id0).await.unwrap().as_deref(),
            Some(&b"same bytes"[..])
        );
        assert_eq!(second.list().await.unwrap(), vec![id0]);
    }

    #[tokio::test]
    async fn fs_corrupt_blob_fails_closed_for_get_and_idempotent_put() {
        // Test design — corruption injection at the durable object boundary:
        // Present(hash(A),A) --external bit rot--> Present(hash(A),B). Both get
        // and the deduplicating fast-path put(A) must reject; neither may bless B
        // under A's address or silently report a successful durable write.
        let tmp = tempfile::tempdir().unwrap();
        let store = FsFileStore::open(tmp.path()).await.unwrap();
        let id = store.put(b"expected bytes").await.unwrap();
        tokio::fs::write(tmp.path().join(&id), b"corrupt bytes")
            .await
            .unwrap();

        assert!(store.get(&id).await.is_err());
        assert!(store.put(b"expected bytes").await.is_err());
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

    #[cfg(feature = "postgres")]
    async fn isolated_postgres_url(schema: &'static str) -> Option<String> {
        use sqlx::Executor;

        let base = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
        let Ok(admin) = sqlx::PgPool::connect(&base).await else {
            println!("[skip] no Postgres reachable");
            return None;
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        let separator = if base.contains('?') { '&' } else { '?' };
        Some(format!(
            "{base}{separator}options=-c%20search_path%3D{schema}"
        ))
    }

    /// Live Postgres round-trip for [`PgFileStore`](crate::postgres::PgFileStore),
    /// isolated in its own schema. Skips when no Postgres is reachable
    /// (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle renders and
    /// runs on Postgres too.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_round_trip() {
        use crate::postgres::PgFileStore;

        let Some(url) = isolated_postgres_url("t_file_store").await else {
            return;
        };

        // Causal graph: verify -> read ledger -> serve/fail; only migrate may
        // create ledger/tables. Portable INTEGER -> Postgres BIGINT, and a
        // catalog write/read must bind and decode both boolean columns as i64.
        // Effects: startup stays read-only or migrates as selected; the exact
        // rendered column types are BIGINT; catalog records round-trip. The
        // table pins all three startup decisions:
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
        let schema_pool = sqlx::PgPool::connect(&url).await.unwrap();
        let boolean_column_types: Vec<String> = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_schema='t_file_store' AND table_name='file_store_file' \
               AND column_name IN ('downloadable', 'deleted') ORDER BY column_name",
        )
        .fetch_all(&schema_pool)
        .await
        .unwrap();
        assert_eq!(boolean_column_types, vec!["bigint", "bigint"]);
        schema_pool.close().await;
        round_trip(&store).await;
        catalog_contract(&store).await;
        // Same id as the core, across the network backend too.
        assert_eq!(
            store.put(b"portable").await.unwrap(),
            InMemoryFileStore::new().put(b"portable").await.unwrap()
        );
    }

    /// Live upgrade proof for the portable-integer renderer transition. The
    /// V1-V3 ledger checksum never encoded the rendered PostgreSQL width, so a
    /// released database may carry the same receipts with `INT4` while a fresh
    /// database now renders the columns as `INT8`.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_v4_widens_legacy_file_flags_without_losing_values() {
        use crate::postgres::PgFileStore;
        use crate::schema::{BUNDLE_ID, NS, file_store_bundle};
        use awaken_scoped_migration::{MigrationBundle, postgres::PostgresMigrationRunner};
        use sqlx::Row;

        // Width-upgrade cause/effect decision table:
        // C1=V1-V3 canonical receipts with legacy INT4 columns; C2=Verify or
        // Migrate; C3=V4 already present. Effects: E1 Verify reports pending and
        // writes nothing; E2 Migrate widens both flags while preserving values;
        // E3 Verify/replay succeeds and performs no second migration.
        //
        // | Rule | C1 | operation | C3 | Effect |
        // |---|---|---|---|---|
        // | W1 | T | Verify  | F | E1 pending, INT4/value unchanged |
        // | W2 | T | Migrate | F | E2 one V4, INT8/value preserved |
        // | W3 | T | Verify/replay | T | E3 current, no DDL |
        let Some(url) = isolated_postgres_url("t_file_store_integer_width").await else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let full = file_store_bundle().unwrap();
        let legacy = MigrationBundle::new(BUNDLE_ID, full.migrations()[..3].to_vec()).unwrap();
        let through_v4 = MigrationBundle::new(BUNDLE_ID, full.migrations()[..4].to_vec()).unwrap();
        let runner = PostgresMigrationRunner::with_prefix(pool.clone(), NS).unwrap();
        assert_eq!(runner.run_bundle(&legacy).await.unwrap().len(), 3);

        // Reproduce the released physical schema while retaining the exact
        // canonical V1-V3 receipts. The portable template/checksum did not
        // change when the renderer began widening INTEGER for fresh databases.
        sqlx::raw_sql(
            "ALTER TABLE file_store_file ALTER COLUMN downloadable TYPE INTEGER \
             USING downloadable::INTEGER; \
             ALTER TABLE file_store_file ALTER COLUMN deleted TYPE INTEGER \
             USING deleted::INTEGER",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO file_store_file \
             (id, workspace_id, blob_id, filename, mime_type, size_bytes, created_at, \
              downloadable, deleted) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind("legacy-file")
        .bind("legacy-workspace")
        .bind("legacy-blob")
        .bind("legacy.txt")
        .bind("text/plain")
        .bind(3_i64)
        .bind("2026-08-27T00:00:00Z")
        .bind(1_i32)
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            runner.verify_bundle(&through_v4).await.is_err(),
            "W1 pending V4"
        );
        let before = sqlx::query("SELECT downloadable, deleted FROM file_store_file WHERE id = $1")
            .bind("legacy-file")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before.get::<i32, _>("downloadable"), 1, "W1");
        assert_eq!(before.get::<i32, _>("deleted"), 0, "W1");

        assert_eq!(runner.run_bundle(&through_v4).await.unwrap().len(), 1, "W2");
        runner.verify_bundle(&through_v4).await.unwrap();
        assert!(
            runner.run_bundle(&through_v4).await.unwrap().is_empty(),
            "W3"
        );
        let column_types = sqlx::query_scalar::<_, String>(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'file_store_file' \
             AND column_name IN ('downloadable', 'deleted') ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(column_types, vec!["bigint", "bigint"], "W2");

        assert_eq!(
            runner.run_bundle(&full).await.unwrap().len(),
            1,
            "V5 terminal association is a separate additive migration"
        );
        runner.verify_bundle(&full).await.unwrap();

        let store = PgFileStore::with_existing_pool(pool).await.unwrap();
        let record = store
            .get_file("legacy-workspace", "legacy-file", false)
            .await
            .unwrap()
            .expect("W2 preserved logical File");
        assert!(record.downloadable, "W2");
        assert!(!record.deleted, "W2");
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
