// @formal entrypoint memory.absent_conditional_delete_is_idempotent
// @formal entrypoint memory.cas_has_one_winner_per_generation
// @formal entrypoint memory.idempotent_write_does_not_advance_generation
// @formal entrypoint memory.matched_conditional_delete_removes_exact_head
// @formal entrypoint memory.rename_replace_preserves_source_identity
// @formal entrypoint memory.stale_cas_never_clobbers_live_content
// @formal entrypoint memory.stale_conditional_delete_never_removes_live_head
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
    MemoryEntry, MemoryGeneration, MemoryPurgeSummary, MemoryRepository, MemoryVersion,
    MemoryVersionOperation,
};

pub use awaken_resource_contract::memory_sha256_hex as sha256_hex;

#[cfg(any(
    test,
    feature = "test-support",
    feature = "sqlite",
    feature = "postgres"
))]
pub(crate) fn validate_copy_conflict(
    candidate: &awaken_resource_contract::MemoryCopyConflict,
) -> Result<(), MemErr> {
    validate_path(&candidate.path)?;
    if let Some(content) = &candidate.content {
        validate_size(content)?;
    }
    if candidate.base.as_ref().is_some_and(|base| {
        base.id.is_empty()
            || base.id.len() > MAX_PATH_BYTES
            || base.version == 0
            || base.content_sha256.len() != 64
            || !base
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }) || (candidate.base.is_none() && candidate.content.is_none())
    {
        return Err(MemErr::Storage("invalid Memory copy proposal base".into()));
    }
    Ok(())
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn decode_copy_conflict(
    id: &str,
    encoded: &str,
) -> Result<awaken_resource_contract::MemoryCopyConflict, MemErr> {
    let candidate = serde_json::from_str(encoded)
        .map_err(|_| MemErr::Storage("invalid stored Memory copy proposal".into()))?;
    validate_copy_conflict(&candidate)?;
    if candidate.id() != id {
        return Err(MemErr::Storage(
            "stored Memory copy proposal identity mismatch".into(),
        ));
    }
    Ok(candidate)
}

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

/// The single admission owner for a write against an observed Memory head.
/// Backends retain their lock/transaction and byte persistence; this decision
/// determines whether those effects may begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadUpdateAdmission {
    Conflict,
    Current,
    Apply,
}

impl HeadUpdateAdmission {
    pub(crate) fn new(
        expected_path_matches: bool,
        identity_matches: bool,
        version_matches: bool,
        content_matches: bool,
        already_current: bool,
    ) -> Self {
        if !expected_path_matches || !identity_matches || !version_matches || !content_matches {
            Self::Conflict
        } else if already_current {
            Self::Current
        } else {
            Self::Apply
        }
    }
}

/// Select the surviving identity for the sole rename-replace transition.
///
/// `T` has no trait bound, so this owner cannot inspect, derive, or substitute
/// either identity. It moves the source identity through unchanged regardless
/// of whether a destination was displaced. Every backend consumes this result
/// for its stored head, version history, and returned [`Memory`].
pub(crate) fn rename_replace_identity<T>(source_id: T, _displaced_id: Option<&T>) -> T {
    source_id
}

/// The single admission owner for deletion against a path-selected head.
/// `None` means the selected path is absent; `Some` carries the exact id/hash
/// identity, version and hash comparisons made from the head observed under
/// the backend's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionalDeleteAdmission {
    Absent,
    Conflict,
    Delete,
}

impl ConditionalDeleteAdmission {
    pub(crate) fn new(current_matches: Option<(bool, bool, bool)>) -> Self {
        match current_matches {
            None => Self::Absent,
            Some((true, true, true)) => Self::Delete,
            Some(_) => Self::Conflict,
        }
    }
}

#[cfg(any(test, kani))]
fn check_head_update_admission(
    expected_path_matches: bool,
    identity_matches: bool,
    version_matches: bool,
    content_matches: bool,
    already_current: bool,
) {
    // Causes: C1 the materialized path still identifies this head; C2-C4 the
    // supplied immutable id, monotonic version and content hash each match the
    // same observed head; C5 requested path/content are already current.
    // Effects and decision rules:
    // U1 any of !C1..!C4 => Conflict; U2 C1..C4,C5 => Current;
    // U3 C1..C4,!C5 => Apply. A same-content ABA is U1, never a retry.
    let decision = HeadUpdateAdmission::new(
        expected_path_matches,
        identity_matches,
        version_matches,
        content_matches,
        already_current,
    );
    let complete_generation =
        expected_path_matches && identity_matches && version_matches && content_matches;
    match decision {
        HeadUpdateAdmission::Conflict => {
            assert!(!complete_generation);
        }
        HeadUpdateAdmission::Current => {
            assert!(complete_generation && already_current);
        }
        HeadUpdateAdmission::Apply => {
            assert!(complete_generation && !already_current);
        }
    }
}

#[cfg(any(test, kani))]
fn check_conditional_delete_admission(current_matches: Option<(bool, bool, bool)>) {
    // Causes: C1 a head exists at the selected path; C2 id matches; C3 version
    // matches; C4 hash matches. Effects: D1 !C1 => Absent; D2 C1 and any of
    // !C2..!C4 => Conflict; D3 C1,C2,C3,C4 => Delete. Only D3 authorizes effects.
    let decision = ConditionalDeleteAdmission::new(current_matches);
    match decision {
        ConditionalDeleteAdmission::Absent => assert!(current_matches.is_none()),
        ConditionalDeleteAdmission::Conflict => assert!(matches!(
            current_matches,
            Some((false, _, _)) | Some((true, false, _)) | Some((true, true, false))
        )),
        ConditionalDeleteAdmission::Delete => {
            assert_eq!(current_matches, Some((true, true, true)));
        }
    }
}

#[cfg(kani)]
/// @obligation memory.cas_has_one_winner_per_generation
/// @obligation memory.idempotent_write_does_not_advance_generation
/// @obligation memory.stale_cas_never_clobbers_live_content
#[kani::proof]
fn memory_head_update_admission_preserves_stale_and_idempotent_heads() {
    // No assumptions: all 32 path/identity/version/hash/current classes pass
    // through the exact production decision used by every backend. Locking, row
    // selection, hash computation and persistence remain outside this proof.
    check_head_update_admission(
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
    );
}

#[cfg(kani)]
/// @obligation memory.absent_conditional_delete_is_idempotent
/// @obligation memory.matched_conditional_delete_removes_exact_head
/// @obligation memory.stale_conditional_delete_never_removes_live_head
#[kani::proof]
fn memory_conditional_delete_admission_is_exact() {
    // No assumptions: None and every present id/hash comparison class pass
    // through the production decision used by every backend. Backend atomicity
    // and history persistence remain outside this decision proof.
    check_conditional_delete_admission(kani::any());
}

#[cfg(kani)]
/// @obligation memory.rename_replace_preserves_source_identity
#[kani::proof]
fn memory_rename_replace_preserves_source_identity() {
    // Causes: C1 arbitrary source identity; C2 destination absent/present; C3
    // arbitrary displaced identity. Effect: the survivor is exactly C1 for
    // every row. No assumption, String model, collection bound, or backend stub
    // is used; the no-trait-bound production owner is representation agnostic.
    let source_id: u64 = kani::any();
    let displaced_id: Option<u64> = kani::any();
    assert_eq!(
        rename_replace_identity(source_id, displaced_id.as_ref()),
        source_id
    );
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
    copy_conflicts:
        BTreeMap<String, BTreeMap<String, awaken_resource_contract::MemoryCopyConflict>>,
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
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        validate_copy_conflict(candidate)?;
        let mut state = self.inner.lock().unwrap();
        let candidates = state.copy_conflicts.entry(store.to_owned()).or_default();
        let id = candidate.id();
        if let Some(existing) = candidates.get(&id) {
            return if existing == candidate {
                Ok(())
            } else {
                Err(MemErr::Storage(
                    "Memory copy proposal identity mismatch".into(),
                ))
            };
        }
        if candidates.len() >= MAX_MEMORIES_PER_STORE {
            return Err(MemErr::AtCapacity);
        }
        candidates.insert(id, candidate.clone());
        Ok(())
    }

    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .copy_conflicts
            .get(store)
            .map(|values| values.values().cloned().collect())
            .unwrap_or_default())
    }
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
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as(store, base, content, target_path, None)
            .await
    }

    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, base, content, target_path, actor, None)
            .await
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, base, content, None, None, Some(path))
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
        record.id = rename_replace_identity(record.id, displaced.as_ref().map(|head| &head.id));
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
        base: &MemoryGeneration,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as(store, path, base, None).await
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let mut guard = self.inner.lock().unwrap();
        let current = guard
            .records
            .get(store)
            .and_then(|records| records.get(path))
            .cloned();
        match ConditionalDeleteAdmission::new(current.as_ref().map(|head| {
            (
                head.id == base.memory_id,
                head.version == base.version,
                head.sha == base.content_sha256,
            )
        })) {
            ConditionalDeleteAdmission::Absent => return Ok(false),
            ConditionalDeleteAdmission::Conflict => {
                return Err(MemErr::Conflict {
                    current: Box::new(
                        current
                            .expect("conflict requires the observed head")
                            .to_memory(true),
                    ),
                });
            }
            ConditionalDeleteAdmission::Delete => {}
        }
        let current = current.expect("delete admission requires the observed head");
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
        let copy_conflicts_deleted = state
            .copy_conflicts
            .remove(store)
            .map_or(0, |values| values.len() as u64);
        Ok(MemoryPurgeSummary {
            heads_deleted,
            versions_deleted,
            copy_conflicts_deleted,
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
        base: &MemoryGeneration,
        content: &str,
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
            .ok_or_else(|| MemErr::NotFound(base.memory_id.clone()))?;
        let source_path = store_map
            .iter()
            .find_map(|(path, record)| (record.id == base.memory_id).then(|| path.clone()))
            .ok_or_else(|| MemErr::NotFound(base.memory_id.clone()))?;
        let requested_path = target_path.unwrap_or(&source_path);
        let current = store_map
            .get(&source_path)
            .expect("source path was discovered under the same lock");
        let already_current = current.sha == new_sha && source_path == requested_path;
        match HeadUpdateAdmission::new(
            expected_path.is_none_or(|path| path == source_path),
            current.id == base.memory_id,
            current.version == base.version,
            current.sha == base.content_sha256,
            already_current,
        ) {
            HeadUpdateAdmission::Conflict => {
                return Err(MemErr::Conflict {
                    current: Box::new(current.to_memory(true)),
                });
            }
            HeadUpdateAdmission::Current => {
                return Ok(current.to_memory(true));
            }
            HeadUpdateAdmission::Apply => {}
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

    #[test]
    fn head_mutation_admission_decision_tables_cover_every_cause_partition() {
        // U1-U4 and D1-D3 above are the complete finite Boolean quotients used
        // by the three repositories. Native enumeration cross-checks the same
        // production functions independently from the symbolic harnesses.
        for expected_path_matches in [false, true] {
            for base_matches in [false, true] {
                for version_matches in [false, true] {
                    for content_matches in [false, true] {
                        for already_current in [false, true] {
                            check_head_update_admission(
                                expected_path_matches,
                                base_matches,
                                version_matches,
                                content_matches,
                                already_current,
                            );
                        }
                    }
                }
            }
        }
        check_conditional_delete_admission(None);
        for id_matches in [false, true] {
            for version_matches in [false, true] {
                for sha_matches in [false, true] {
                    check_conditional_delete_admission(Some((
                        id_matches,
                        version_matches,
                        sha_matches,
                    )));
                }
            }
        }
    }

    #[test]
    fn rename_replace_identity_decision_table_keeps_only_the_source() {
        // Causes: C1 source id at zero/ordinary/u64::MAX; C2 destination absent;
        // C3 destination present with the same/different id. Effect E1 always
        // returns exactly C1. R1=C2->E1; R2=C3+same->E1;
        // R3=C3+different->E1. The production function is type-parametric;
        // these boundary values independently cross-check its Kani instance.
        for source in [0, 7, u64::MAX] {
            for displaced in [None, Some(source), Some(source.wrapping_add(1))] {
                assert_eq!(rename_replace_identity(source, displaced.as_ref()), source);
            }
        }
    }

    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    #[test]
    fn stored_copy_candidate_requires_exact_identity_and_valid_content() {
        // RC-CODEC exact id+valid payload => readable; altered content, id,
        // malformed JSON or invalid domain path => fail closed, never present
        // corrupted content as a durable conflict receipt.
        let candidate = awaken_resource_contract::MemoryCopyConflict {
            path: "/note.md".into(),
            base: None,
            content: Some("exact".into()),
        };
        let encoded = serde_json::to_string(&candidate).unwrap();
        assert_eq!(
            decode_copy_conflict(&candidate.id(), &encoded).unwrap(),
            candidate
        );
        assert!(decode_copy_conflict("foreign-id", &encoded).is_err());
        for encoded in [
            encoded.replace("exact", "altered"),
            "{broken".into(),
            encoded.replace("/note.md", "../escape"),
        ] {
            assert!(decode_copy_conflict(&candidate.id(), &encoded).is_err());
        }
    }
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
            .update(store, &created.generation(), "two")
            .await
            .unwrap();
        assert_eq!(fs.list_versions(store).await.unwrap().len(), 2);
        fs.update(store, &updated.generation(), "two")
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
        fs.update("purge", &updated.generation(), "two")
            .await
            .unwrap();
        assert_eq!(
            fs.purge_store("purge").await.unwrap(),
            MemoryPurgeSummary {
                heads_deleted: 1,
                versions_deleted: 2,
                copy_conflicts_deleted: 0,
            }
        );
        assert!(fs.list("purge", "/").await.unwrap().is_empty());
        assert!(fs.list_versions("purge").await.unwrap().is_empty());
        assert_eq!(
            fs.purge_store("purge").await.unwrap(),
            MemoryPurgeSummary {
                heads_deleted: 0,
                versions_deleted: 0,
                copy_conflicts_deleted: 0,
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
        fs.update("snapshot", &a.generation(), "new-a")
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
            .update("s", &m.generation(), "updated")
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
            .update("capacity", &first.generation(), "updated")
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
