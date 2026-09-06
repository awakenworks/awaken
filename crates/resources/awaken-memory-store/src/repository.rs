//! Path-addressed, CAS memory model (ADR-0053, D1).
//!
//! The [`MemoryRepository`] port projects a memory store as a set of **path-addressed
//! memory files**, each carrying a `content_sha256`, a monotonic per-path
//! `version`, and real create/update timestamps — the model a write-through FUSE
//! mount ([ADR-0053] D2) needs to give an LLM native `read`/`write`/`edit`/`grep`
//! semantics over `/mnt/memory/{store}/*`. A store holds many files with **per-file
//! optimistic concurrency**: [`MemoryRepository::update`] is a compare-and-swap on the base
//! sha, so two writers never silently clobber each other.
//!
//! Backends: `VolatileMemoryRepository` behind explicit test support and
//! feature-gated SQLite/Postgres transactional repositories for durable execution.
//!
#[cfg(any(test, feature = "test-support"))]
use std::collections::BTreeMap;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(test, feature = "test-support"))]
use async_trait::async_trait;

// The path-addressed memory port (`MemoryRepository`), its value types (`Memory`,
// `MemoryEntry`), the error (`MemErr`), and the size caps live in the port-only
// contract crate. This module implements the port and re-exports them so
// `awaken_memory_store::repository::Memory` and root re-exports resolve uniformly.
pub use awaken_resource_contract::{
    MAX_MEMORIES_PER_STORE, MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, Memory, MemoryActor,
    MemoryEntry, MemoryPurgeSummary, MemoryRepository, MemoryVersion, MemoryVersionOperation,
};

pub use awaken_resource_contract::memory_sha256_hex as sha256_hex;

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
pub(crate) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Validate a memory path: absolute, non-root, no `//`, no `.`/`..` segment, no
/// control chars, ≤ [`MAX_PATH_BYTES`].
#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
pub(crate) fn validate_path(path: &str) -> Result<(), MemErr> {
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

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
pub(crate) fn validate_size(content: &str) -> Result<(), MemErr> {
    if content.len() > MAX_MEMORY_BYTES {
        Err(MemErr::TooLarge)
    } else {
        Ok(())
    }
}

/// The in-memory aggregate record.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
struct Record {
    id: String,
    path: String,
    content: String,
    sha: String,
    version: u64,
    created: u128,
    updated: u128,
}

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
pub(crate) fn under_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        return true;
    }
    let p = prefix.trim_end_matches('/');
    path == p || path.starts_with(&format!("{p}/"))
}

// ---------------------------------------------------------------------------
// In-memory backend
// ---------------------------------------------------------------------------

/// In-memory [`MemoryRepository`] for tests and scenario fixtures. One lock guards
/// the whole map, so every create/update/rename/delete is atomic and CAS is race-free.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct VolatileMemoryRepository {
    inner: Mutex<InMemoryState>,
    next: AtomicU64,
    version_next: AtomicU64,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct InMemoryState {
    // store id → (path → record)
    records: BTreeMap<String, BTreeMap<String, Record>>,
    versions: BTreeMap<String, Vec<MemoryVersion>>,
}

#[cfg(any(test, feature = "test-support"))]
impl VolatileMemoryRepository {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn mint_id(&self) -> String {
        format!("mem_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn append_version(
        &self,
        state: &mut InMemoryState,
        store: &str,
        record: &Record,
        operation: MemoryVersionOperation,
        content: Option<String>,
        actor: Option<&MemoryActor>,
    ) {
        let ordinal = self.version_next.fetch_add(1, Ordering::SeqCst) + 1;
        state
            .versions
            .entry(store.to_string())
            .or_default()
            .push(MemoryVersion {
                id: format!("memver_{ordinal}"),
                memory_id: record.id.clone(),
                operation,
                path: record.path.clone(),
                content,
                created_unix_nanos: record.updated,
                created_by: actor.cloned(),
                redacted_unix_nanos: None,
                redacted_by: None,
            });
    }
}

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
impl MemoryRepository for VolatileMemoryRepository {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .records
            .get(store)
            .map(|records| {
                records
                    .values()
                    .map(|record| record.to_memory(true))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .records
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
            .records
            .get(store)
            .and_then(|s| s.get(path))
            .map(|r| r.to_memory(true)))
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        self.create_as(store, path, content, None).await
    }

    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let id = self.mint_id();
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard.records.entry(store.to_string()).or_default();
        if store_map.contains_key(path) {
            return Err(MemErr::PathConflict(path.to_string()));
        }
        if store_map.len() >= MAX_MEMORIES_PER_STORE {
            return Err(MemErr::AtCapacity);
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
        store_map.insert(path.to_string(), record.clone());
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Created,
            Some(content.to_string()),
            actor,
        );
        Ok(memory)
    }

    async fn update_head(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as(store, id, content, base_sha, target_path, None)
            .await
    }

    async fn update_head_as(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, id, content, base_sha, target_path, actor, None)
            .await
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, id, content, base_sha, None, None, Some(path))
            .await
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard
            .records
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
        // Atomic replace of the destination (POSIX rename). Preserve a deletion
        // version for the displaced memory before recording the moved head.
        let displaced = store_map.remove(to);
        record.path = to.to_string();
        record.version += 1;
        record.updated = now_nanos();
        let memory = record.to_memory(true);
        store_map.insert(to.to_string(), record.clone());
        if let Some(displaced) = displaced {
            self.append_version(
                &mut guard,
                store,
                &displaced,
                MemoryVersionOperation::Deleted,
                None,
                None,
            );
        }
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Modified,
            Some(record.content.clone()),
            None,
        );
        Ok(memory)
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        self.delete_by_path_as(store, path, None).await
    }

    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        let mut guard = self.inner.lock().unwrap();
        let removed = guard
            .records
            .get_mut(store)
            .and_then(|records| records.remove(path));
        if let Some(record) = removed {
            self.append_version(
                &mut guard,
                store,
                &record,
                MemoryVersionOperation::Deleted,
                None,
                actor,
            );
        }
        Ok(())
    }

    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as(store, path, base_id, base_sha, None)
            .await
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let mut guard = self.inner.lock().unwrap();
        let Some(current) = guard
            .records
            .get(store)
            .and_then(|records| records.get(path))
            .cloned()
        else {
            return Ok(false);
        };
        if current.id != base_id || current.sha != base_sha {
            return Err(MemErr::Conflict {
                current: Box::new(current.to_memory(true)),
            });
        }
        guard
            .records
            .get_mut(store)
            .expect("the store was observed under the same lock")
            .remove(path);
        self.append_version(
            &mut guard,
            store,
            &current,
            MemoryVersionOperation::Deleted,
            None,
            actor,
        );
        Ok(true)
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .versions
            .get(store)
            .cloned()
            .unwrap_or_default())
    }

    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.redact_version_as(store, version_id, None).await
    }

    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        let mut guard = self.inner.lock().unwrap();
        let Some(version) = guard
            .versions
            .get_mut(store)
            .and_then(|versions| versions.iter_mut().find(|version| version.id == version_id))
        else {
            return Ok(None);
        };
        if version.redacted_unix_nanos.is_none() {
            version.redacted_unix_nanos = Some(now_nanos());
            version.redacted_by = actor.cloned();
            version.content = None;
        }
        Ok(Some(version.clone()))
    }

    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        let mut state = self.inner.lock().unwrap();
        let heads_deleted = state
            .records
            .remove(store)
            .map_or(0, |records| records.len() as u64);
        let versions_deleted = state
            .versions
            .remove(store)
            .map_or(0, |versions| versions.len() as u64);
        Ok(MemoryPurgeSummary {
            heads_deleted,
            versions_deleted,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl VolatileMemoryRepository {
    // One mutation/history implementation for ID-following and path-pinned updates.
    #[allow(clippy::too_many_arguments)]
    async fn update_head_checked(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
        expected_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        if let Some(path) = expected_path {
            validate_path(path)?;
        }
        if let Some(path) = target_path {
            validate_path(path)?;
        }
        let new_sha = sha256_hex(content);
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard
            .records
            .get_mut(store)
            .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        let source_path = store_map
            .iter()
            .find_map(|(path, record)| (record.id == id).then(|| path.clone()))
            .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        let requested_path = target_path.unwrap_or(&source_path);
        let current = store_map
            .get(&source_path)
            .expect("source path was discovered under the same lock");
        let already_current = current.sha == new_sha && source_path == requested_path;
        if expected_path.is_some_and(|path| path != source_path) {
            return Err(MemErr::Conflict {
                current: Box::new(current.to_memory(true)),
            });
        }
        if current.sha != base_sha {
            if already_current {
                return Ok(current.to_memory(true));
            }
            return Err(MemErr::Conflict {
                current: Box::new(current.to_memory(true)),
            });
        }
        if already_current {
            return Ok(current.to_memory(true));
        }

        let mut record = store_map
            .remove(&source_path)
            .expect("source path was discovered under the same lock");
        let displaced = if requested_path == source_path {
            None
        } else {
            store_map.remove(requested_path)
        };
        record.path = requested_path.to_string();
        record.content = content.to_string();
        record.sha = new_sha;
        record.version += 1;
        record.updated = now_nanos();
        let memory = record.to_memory(true);
        store_map.insert(record.path.clone(), record.clone());
        if let Some(displaced) = displaced {
            self.append_version(
                &mut guard,
                store,
                &displaced,
                MemoryVersionOperation::Deleted,
                None,
                actor,
            );
        }
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Modified,
            Some(content.to_string()),
            actor,
        );
        Ok(memory)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::conformance::{
        atomic_head_update_conformance, concurrent_cas_one_winner, concurrent_create_one_winner,
        conformance, extended_conformance, ids_stay_monotonic_across_delete, not_found_paths,
        rename_replace_orphans_the_destination_id,
    };

    /// Cause-effect graph for the aggregate invariant:
    ///
    /// state-changing mutation => one durable version (rename-replace => displaced
    /// delete + source modify); rejected/idempotent mutation => no version;
    /// redaction => history content removed, live head unchanged.
    async fn history_conformance(fs: &dyn MemoryRepository) {
        let store = "history";
        let created = fs.create(store, "/a.md", "one").await.unwrap();
        let first = fs.list_versions(store).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].memory_id, created.id);
        assert_eq!(first[0].operation, MemoryVersionOperation::Created);
        assert_eq!(first[0].content.as_deref(), Some("one"));

        assert!(matches!(
            fs.create(store, "/a.md", "duplicate").await,
            Err(MemErr::PathConflict(_))
        ));
        assert_eq!(fs.list_versions(store).await.unwrap().len(), 1);

        let updated = fs
            .update(store, &created.id, "two", &created.content_sha256)
            .await
            .unwrap();
        assert_eq!(fs.list_versions(store).await.unwrap().len(), 2);
        fs.update(store, &created.id, "two", "stale-but-idempotent")
            .await
            .unwrap();
        assert_eq!(
            fs.list_versions(store).await.unwrap().len(),
            2,
            "idempotent CAS does not invent a version"
        );

        let displaced = fs.create(store, "/b.md", "old").await.unwrap();
        fs.rename(store, "/a.md", "/b.md").await.unwrap();
        let after_rename = fs.list_versions(store).await.unwrap();
        assert_eq!(after_rename.len(), 5);
        assert_eq!(after_rename[3].memory_id, displaced.id);
        assert_eq!(after_rename[3].operation, MemoryVersionOperation::Deleted);
        assert_eq!(after_rename[4].memory_id, created.id);
        assert_eq!(after_rename[4].operation, MemoryVersionOperation::Modified);

        fs.delete_by_path(store, "/b.md").await.unwrap();
        fs.delete_by_path(store, "/b.md").await.unwrap();
        let versions = fs.list_versions(store).await.unwrap();
        assert_eq!(versions.len(), 6, "idempotent delete appends once");

        let redacted = fs
            .redact_version(store, &first[0].id)
            .await
            .unwrap()
            .unwrap();
        assert!(redacted.content.is_none());
        assert!(redacted.redacted_unix_nanos.is_some());
        let again = fs
            .redact_version(store, &first[0].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, redacted, "redaction is idempotent");
        assert!(
            fs.redact_version("other", &first[0].id)
                .await
                .unwrap()
                .is_none(),
            "a version id cannot cross its store boundary"
        );
        assert_eq!(updated.content.as_deref(), Some("two"));
    }

    async fn purge_conformance(fs: &dyn MemoryRepository) {
        fs.create("purge", "/a.md", "one").await.unwrap();
        let updated = fs.get_by_path("purge", "/a.md").await.unwrap().unwrap();
        fs.update("purge", &updated.id, "two", &updated.content_sha256)
            .await
            .unwrap();
        assert_eq!(
            fs.purge_store("purge").await.unwrap(),
            MemoryPurgeSummary {
                heads_deleted: 1,
                versions_deleted: 2,
            }
        );
        assert!(fs.list("purge", "/").await.unwrap().is_empty());
        assert!(fs.list_versions("purge").await.unwrap().is_empty());
        assert_eq!(
            fs.purge_store("purge").await.unwrap(),
            MemoryPurgeSummary {
                heads_deleted: 0,
                versions_deleted: 0,
            }
        );
    }

    /// Snapshot cause/effect design:
    /// C1 multiple path heads exist -> E1 one path-ordered, content-bearing image;
    /// C2 a later head mutation occurs -> E2 the previously returned value stays
    /// frozen while a new snapshot observes the mutation. These are the two rules
    /// that prevent a consolidation input from becoming a mixed `list`/`get` view.
    async fn snapshot_heads_conformance(fs: &dyn MemoryRepository) {
        fs.create("snapshot", "/z.md", "old-z").await.unwrap();
        let a = fs.create("snapshot", "/a.md", "old-a").await.unwrap();
        let frozen = fs.snapshot_heads("snapshot").await.unwrap();
        assert_eq!(
            frozen
                .iter()
                .map(|memory| memory.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/a.md", "/z.md"]
        );
        assert_eq!(frozen[0].content.as_deref(), Some("old-a"));
        fs.update("snapshot", &a.id, "new-a", &a.content_sha256)
            .await
            .unwrap();
        assert_eq!(frozen[0].content.as_deref(), Some("old-a"));
        assert_eq!(
            fs.snapshot_heads("snapshot").await.unwrap()[0]
                .content
                .as_deref(),
            Some("new-a")
        );
    }

    #[tokio::test]
    async fn in_memory_extended_conformance() {
        extended_conformance(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_history_conformance() {
        history_conformance(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_purge_conformance() {
        purge_conformance(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_atomic_head_update_conformance() {
        atomic_head_update_conformance(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_snapshot_heads_conformance() {
        snapshot_heads_conformance(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_not_found_paths() {
        not_found_paths(&VolatileMemoryRepository::new()).await;
    }

    #[tokio::test]
    async fn in_memory_backend_conforms() {
        conformance(&VolatileMemoryRepository::new()).await;
    }

    /// Ids are monotonic across a delete: deleting the highest-ordinal memory and then
    /// creating must mint a FRESH id, never re-hand-out the deleted one. Holds within a
    /// live handle for the in-memory backend and durably for every persistent
    /// backend because their high-water counters only advance.

    #[tokio::test]
    async fn in_memory_ids_stay_monotonic_across_delete() {
        ids_stay_monotonic_across_delete(&VolatileMemoryRepository::new()).await;
    }

    /// A rename-replace masks the destination entirely: after moving `src` over `dst`,
    /// the memory formerly at `dst` (its id) is gone — an update on that stale id is
    /// `NotFound`, and the destination path now carries the source's id and content.
    /// This is the "who masks whom" edge of the POSIX-replace contract.

    #[tokio::test]
    async fn in_memory_rename_replace_orphans_destination_id() {
        rename_replace_orphans_the_destination_id(&VolatileMemoryRepository::new()).await;
    }

    // --- Concurrency (ADR-0053 P2.5): the store is the shared source of truth
    //     under concurrent mounts, and writes are CAS-serialized. ---

    #[tokio::test]
    async fn concurrent_create_of_the_same_path_has_exactly_one_winner() {
        concurrent_create_one_winner(std::sync::Arc::new(VolatileMemoryRepository::new())).await;
    }

    #[tokio::test]
    async fn concurrent_update_on_the_same_base_has_exactly_one_cas_winner() {
        concurrent_cas_one_winner(std::sync::Arc::new(VolatileMemoryRepository::new())).await;
    }

    #[tokio::test]
    async fn a_write_through_one_handle_is_visible_through_another() {
        // Two handles to one store stand in for two mounts sharing the same source
        // of truth — the basis of ADR-0053's shared-mount read coherence.
        let fs = std::sync::Arc::new(VolatileMemoryRepository::new());
        let (writer, reader) = (fs.clone(), fs.clone());
        let m = writer.create("s", "/shared.md", "hello").await.unwrap();
        assert_eq!(
            reader
                .get_by_path("s", "/shared.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("hello"),
            "the second handle sees the create"
        );
        writer
            .update("s", &m.id, "updated", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(
            reader
                .get_by_path("s", "/shared.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("updated"),
            "the second handle sees the update"
        );
    }

    pub(crate) async fn capacity_conformance(fs: &dyn MemoryRepository) {
        // Causes: live-head count at 1,999/2,000, create vs update, then delete.
        // Constraints: versions do not consume capacity and the check shares the
        // atomic repository mutation used by API and write-through mounts.
        // Effects: 2,000th create succeeds; 2,001st is AtCapacity; existing update
        // succeeds; deleting one head permits exactly one replacement create.
        // Decision rule: Memory capacity MC1-MC4.
        let first = fs.create("capacity", "/m0.md", "zero").await.unwrap();
        for index in 1..MAX_MEMORIES_PER_STORE {
            fs.create("capacity", &format!("/m{index}.md"), "x")
                .await
                .unwrap();
        }
        assert!(matches!(
            fs.create("capacity", "/overflow.md", "x").await,
            Err(MemErr::AtCapacity)
        ));
        let updated = fs
            .update("capacity", &first.id, "updated", &first.content_sha256)
            .await
            .unwrap();
        assert_eq!(updated.content.as_deref(), Some("updated"), "MC3");
        fs.delete_by_path("capacity", "/m1.md").await.unwrap();
        assert!(
            fs.create("capacity", "/replacement.md", "x").await.is_ok(),
            "MC4"
        );
    }

    #[tokio::test]
    async fn volatile_store_capacity_rejects_only_new_heads_and_reopens_after_delete() {
        capacity_conformance(&VolatileMemoryRepository::new()).await;
    }
}
