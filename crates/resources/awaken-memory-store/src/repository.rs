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
//! Backends: [`VolatileMemoryRepository`] for ephemeral execution and
//! feature-gated SQLite/Postgres transactional repositories for durable execution.
//!
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

// The path-addressed memory port (`MemoryRepository`), its value types (`Memory`,
// `MemoryEntry`), the error (`MemErr`), and the size caps live in the port-only
// contract crate. This module implements the port and re-exports them so
// `awaken_memory_store::repository::Memory` and root re-exports resolve uniformly.
pub use awaken_resource_contract::{
    MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, Memory, MemoryEntry, MemoryPurgeSummary,
    MemoryRepository, MemoryVersion, MemoryVersionOperation,
};

/// Lowercase hex SHA-256 of `content` — the CAS token (Anthropic wire is sha256).
#[must_use]
pub fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Validate a memory path: absolute, non-root, no `//`, no `.`/`..` segment, no
/// control chars, ≤ [`MAX_PATH_BYTES`].
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

pub(crate) fn validate_size(content: &str) -> Result<(), MemErr> {
    if content.len() > MAX_MEMORY_BYTES {
        Err(MemErr::TooLarge)
    } else {
        Ok(())
    }
}

/// The in-memory aggregate record.
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

/// In-memory [`MemoryRepository`] (tests / ephemeral single-process). One lock guards the
/// whole map, so every create/update/rename/delete is atomic and CAS is race-free.
#[derive(Default)]
pub struct VolatileMemoryRepository {
    inner: Mutex<InMemoryState>,
    next: AtomicU64,
    version_next: AtomicU64,
}

#[derive(Default)]
struct InMemoryState {
    // store id → (path → record)
    records: BTreeMap<String, BTreeMap<String, Record>>,
    versions: BTreeMap<String, Vec<MemoryVersion>>,
}

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
                redacted_unix_nanos: None,
            });
    }
}

#[async_trait]
impl MemoryRepository for VolatileMemoryRepository {
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
        validate_path(path)?;
        validate_size(content)?;
        let id = self.mint_id();
        let mut guard = self.inner.lock().unwrap();
        let store_map = guard.records.entry(store.to_string()).or_default();
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
        store_map.insert(path.to_string(), record.clone());
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Created,
            Some(content.to_string()),
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
        validate_size(content)?;
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
            );
        }
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Modified,
            Some(content.to_string()),
        );
        Ok(memory)
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
            );
        }
        self.append_version(
            &mut guard,
            store,
            &record,
            MemoryVersionOperation::Modified,
            Some(record.content.clone()),
        );
        Ok(memory)
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the same conformance suite over any `MemoryRepository` backend.
    async fn conformance(fs: &dyn MemoryRepository) {
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

    /// Cause-effect-graph cases beyond `conformance`: the remaining validation
    /// branches, CAS/rename **precedence** (who masks whom), and prefix-boundary
    /// correctness. Runs over any backend.
    async fn extended_conformance(fs: &dyn MemoryRepository) {
        let store = "ext";

        // G-C1: the other validate_path rejections — root, a control char, a `.`
        // segment, and a path one byte over the cap.
        let over_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES)); // len == cap + 1
        for bad in ["/", "/ctrl\nseg.md", "/a/./b.md", over_cap.as_str()] {
            assert!(
                matches!(
                    fs.create(store, bad, "x").await,
                    Err(MemErr::InvalidPath(_))
                ),
                "expected InvalidPath for {bad:?}"
            );
        }

        // G-C2: size boundary — content exactly at the cap is accepted (only `> cap`
        // is TooLarge, which `conformance` already checks).
        let at_cap = "x".repeat(MAX_MEMORY_BYTES);
        let m_cap = fs.create(store, "/at-cap.md", &at_cap).await.unwrap();
        assert_eq!(m_cap.content_size, MAX_MEMORY_BYTES as u64);

        // G-U1: validate_size runs BEFORE the id lookup, so an oversized update to a
        // nonexistent id reports TooLarge, not NotFound (precedence pin).
        let big = "x".repeat(MAX_MEMORY_BYTES + 1);
        assert!(matches!(
            fs.update(store, "mem_does_not_exist", &big, "sha").await,
            Err(MemErr::TooLarge)
        ));

        // G-U2: an idempotent update (stale base, but content already current) must
        // NOT bump the version.
        let m = fs.create(store, "/idem.md", "v0").await.unwrap();
        let up = fs
            .update(store, &m.id, "v1", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(up.version, 2);
        let idem = fs
            .update(store, &m.id, "v1", "stale-base-sha")
            .await
            .unwrap();
        assert_eq!(
            idem.version, 2,
            "idempotent write leaves the version unchanged"
        );
        assert_eq!(idem.content.as_deref(), Some("v1"));

        // G-R1: a rename to an invalid path fails before touching the source.
        fs.create(store, "/src.md", "s").await.unwrap();
        assert!(matches!(
            fs.rename(store, "/src.md", "relative").await,
            Err(MemErr::InvalidPath(_))
        ));
        assert!(
            fs.get_by_path(store, "/src.md").await.unwrap().is_some(),
            "a rejected rename must not destroy the source"
        );

        // G-R2: rename with from == to returns the current memory and does NOT bump
        // the version (the success shape of from==to, distinct from the NotFound one).
        let src = fs.get_by_path(store, "/src.md").await.unwrap().unwrap();
        let same = fs.rename(store, "/src.md", "/src.md").await.unwrap();
        assert_eq!(same.id, src.id);
        assert_eq!(
            same.version, src.version,
            "from==to leaves the version unchanged"
        );

        // G-R3: a pure move (destination absent) preserves the id and bumps version.
        let moved = fs.rename(store, "/src.md", "/moved.md").await.unwrap();
        assert_eq!(moved.id, src.id, "a move preserves the memory id");
        assert_eq!(moved.version, src.version + 1);
        assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_none());
        assert_eq!(
            fs.get_by_path(store, "/moved.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("s")
        );

        // G-L1: under_prefix respects the path boundary — "/notes" must match "/notes"
        // and "/notes/x.md" but NOT the sibling "/notesbar".
        let pstore = "prefix";
        fs.create(pstore, "/notes", "a").await.unwrap();
        fs.create(pstore, "/notes/x.md", "b").await.unwrap();
        fs.create(pstore, "/notesbar", "c").await.unwrap();
        let mut under: Vec<_> = fs
            .list(pstore, "/notes")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        under.sort();
        assert_eq!(
            under,
            vec!["/notes", "/notes/x.md"],
            "a prefix must not leak across a path boundary"
        );

        // G-L2: an empty prefix lists everything, exactly like "/".
        assert_eq!(fs.list(pstore, "").await.unwrap().len(), 3);

        // delete on a store that was never created is a no-op Ok (idempotent).
        fs.delete_by_path("never_created_store", "/x.md")
            .await
            .unwrap();
    }

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

    /// A public API head update is one aggregate transaction: path validation and
    /// CAS happen before any write; content + rename-replace + history commit once.
    async fn atomic_head_update_conformance(fs: &dyn MemoryRepository) {
        let store = "atomic-head";
        let source = fs.create(store, "/source.md", "v1").await.unwrap();
        let destination = fs.create(store, "/destination.md", "old").await.unwrap();
        let initial_versions = fs.list_versions(store).await.unwrap().len();

        assert!(matches!(
            fs.update_head(
                store,
                &source.id,
                "must-not-commit",
                &source.content_sha256,
                Some("relative.md"),
            )
            .await,
            Err(MemErr::InvalidPath(_))
        ));
        assert_eq!(
            fs.get_by_path(store, "/source.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("v1")
        );
        assert_eq!(
            fs.list_versions(store).await.unwrap().len(),
            initial_versions
        );

        assert!(matches!(
            fs.update_head(store, &source.id, "v1", "stale", Some("/moved.md"),)
                .await,
            Err(MemErr::Conflict { .. })
        ));
        assert!(fs.get_by_path(store, "/source.md").await.unwrap().is_some());
        assert!(fs.get_by_path(store, "/moved.md").await.unwrap().is_none());
        assert_eq!(
            fs.list_versions(store).await.unwrap().len(),
            initial_versions
        );

        let updated = fs
            .update_head(
                store,
                &source.id,
                "v2",
                &source.content_sha256,
                Some("/destination.md"),
            )
            .await
            .unwrap();
        assert_eq!(updated.id, source.id);
        assert_eq!(updated.path, "/destination.md");
        assert_eq!(updated.content.as_deref(), Some("v2"));
        assert_eq!(updated.version, source.version + 1);
        assert!(fs.get_by_path(store, "/source.md").await.unwrap().is_none());
        assert_ne!(updated.id, destination.id);
        let versions = fs.list_versions(store).await.unwrap();
        assert_eq!(versions.len(), initial_versions + 2);
        assert_eq!(
            versions[initial_versions].operation,
            MemoryVersionOperation::Deleted
        );
        assert_eq!(versions[initial_versions].memory_id, destination.id);
        assert_eq!(
            versions[initial_versions + 1].operation,
            MemoryVersionOperation::Modified
        );
        assert_eq!(versions[initial_versions + 1].memory_id, source.id);

        let idempotent = fs
            .update_head(
                store,
                &source.id,
                "v2",
                "stale-after-success",
                Some("/destination.md"),
            )
            .await
            .unwrap();
        assert_eq!(idempotent.version, updated.version);
        assert_eq!(fs.list_versions(store).await.unwrap().len(), versions.len());
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

    /// The `NotFound` paths of `update`/`rename`, over any backend.
    async fn not_found_paths(fs: &dyn MemoryRepository) {
        let store = "s";
        // update on a store that does not exist yet → NotFound.
        assert!(matches!(
            fs.update(store, "no_id", "x", "sha").await,
            Err(MemErr::NotFound(_))
        ));
        // update an unknown id in an existing store → NotFound.
        fs.create(store, "/seed.md", "s").await.unwrap();
        assert!(matches!(
            fs.update(store, "no_such_id", "x", "sha").await,
            Err(MemErr::NotFound(_))
        ));
        // rename a source that does not exist → NotFound (both the from==to and the
        // from!=to shapes).
        assert!(matches!(
            fs.rename(store, "/gone.md", "/x.md").await,
            Err(MemErr::NotFound(_))
        ));
        assert!(matches!(
            fs.rename(store, "/gone.md", "/gone.md").await,
            Err(MemErr::NotFound(_))
        ));
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
    async fn ids_stay_monotonic_across_delete(fs: &dyn MemoryRepository) {
        let a = fs.create("s", "/a.md", "a").await.unwrap();
        let b = fs.create("s", "/b.md", "b").await.unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("mem_1", "mem_2"));
        // Delete the highest-ordinal memory, then create again.
        fs.delete_by_path("s", "/b.md").await.unwrap();
        let c = fs.create("s", "/c.md", "c").await.unwrap();
        assert_eq!(
            c.id, "mem_3",
            "a create after deleting the top id must not reuse it"
        );
    }

    #[tokio::test]
    async fn in_memory_ids_stay_monotonic_across_delete() {
        ids_stay_monotonic_across_delete(&VolatileMemoryRepository::new()).await;
    }

    /// A rename-replace masks the destination entirely: after moving `src` over `dst`,
    /// the memory formerly at `dst` (its id) is gone — an update on that stale id is
    /// `NotFound`, and the destination path now carries the source's id and content.
    /// This is the "who masks whom" edge of the POSIX-replace contract.
    async fn rename_replace_orphans_the_destination_id(fs: &dyn MemoryRepository) {
        let dst = fs.create("s", "/dst.md", "old-dst").await.unwrap();
        let src = fs.create("s", "/src.md", "src").await.unwrap();
        assert_ne!(dst.id, src.id);
        let moved = fs.rename("s", "/src.md", "/dst.md").await.unwrap();
        assert_eq!(moved.id, src.id, "the surviving memory keeps the source id");
        assert_eq!(moved.content.as_deref(), Some("src"));
        // The replaced destination's id no longer addresses anything.
        assert!(
            matches!(
                fs.update("s", &dst.id, "zombie", &dst.content_sha256).await,
                Err(MemErr::NotFound(_))
            ),
            "the replaced destination id must be unaddressable after the move"
        );
        let at_dst = fs.get_by_path("s", "/dst.md").await.unwrap().unwrap();
        assert_eq!(at_dst.id, src.id);
        assert_eq!(at_dst.content.as_deref(), Some("src"));
    }

    #[tokio::test]
    async fn in_memory_rename_replace_orphans_destination_id() {
        rename_replace_orphans_the_destination_id(&VolatileMemoryRepository::new()).await;
    }

    // --- Concurrency (ADR-0053 P2.5): the store is the shared source of truth
    //     under concurrent mounts, and writes are CAS-serialized. ---

    #[tokio::test]
    async fn concurrent_create_of_the_same_path_has_exactly_one_winner() {
        let fs = std::sync::Arc::new(VolatileMemoryRepository::new());
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let fs = fs.clone();
            handles.push(tokio::spawn(async move {
                fs.create("s", "/race.md", &format!("v{i}")).await
            }));
        }
        let (mut oks, mut conflicts) = (0, 0);
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => oks += 1,
                Err(MemErr::PathConflict(_)) => conflicts += 1,
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(oks, 1, "exactly one create wins the path");
        assert_eq!(conflicts, 7);
    }

    #[tokio::test]
    async fn concurrent_update_on_the_same_base_has_exactly_one_cas_winner() {
        let fs = std::sync::Arc::new(VolatileMemoryRepository::new());
        let m = fs.create("s", "/c.md", "v0").await.unwrap();
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let (fs, id, base) = (fs.clone(), m.id.clone(), m.content_sha256.clone());
            handles.push(tokio::spawn(async move {
                fs.update("s", &id, &format!("w{i}"), &base).await
            }));
        }
        let (mut oks, mut conflicts) = (0, 0);
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => oks += 1,
                Err(MemErr::Conflict { .. }) => conflicts += 1,
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(
            oks, 1,
            "exactly one CAS write wins; the rest conflict, none clobbers"
        );
        assert_eq!(conflicts, 7);
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
}
