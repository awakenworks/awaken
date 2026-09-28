// @formal evidence memory.cas_has_one_winner_per_generation
// @formal evidence memory.idempotent_write_does_not_advance_generation
// @formal evidence memory.stale_cas_never_clobbers_live_content
// @formal evidence memory.stale_conditional_delete_never_removes_live_head
//! Copy-in / harvest fallback (ADR-0053 D6) for hosts without FUSE.
//!
//! A write-through FUSE mount is the live path, but it needs `/dev/fuse` — absent on
//! macOS, in CI, and in unprivileged containers. There, the same path-addressed
//! [`MemoryRepository`] store is materialized the old way: [`materialize`] copies every
//! memory out to plain files the agent reads/writes, and [`harvest`] walks those
//! files back into the store after a turn (create new, CAS-update changed, skip
//! unchanged). [`fuse_available`] is the capability check a provider gates on.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use awaken_provisioning_contract::MemoryMaterializationHead;
use awaken_resource_contract::{
    MemErr, MemoryCopyBase, MemoryCopyConflict, MemoryRepository, memory_sha256_hex as sha256_hex,
};

use crate::FuseError;

// One physical projection profile for ingress, recovery and harvest. Logical
// Repository paths/quotas remain owned by resource-contract.
const COPY_LIMITS: awaken_sandbox_fs::TreeReadLimits = awaken_sandbox_fs::TreeReadLimits {
    file_bytes: awaken_resource_contract::MAX_MEMORY_BYTES as u64,
    total_bytes: (awaken_resource_contract::MAX_MEMORY_BYTES
        * awaken_resource_contract::MAX_MEMORIES_PER_STORE) as u64,
    entries: awaken_resource_contract::MAX_MEMORIES_PER_STORE * 2,
    depth: 128,
};

pub(crate) fn validate_copy_entries<'a>(
    entries: impl IntoIterator<Item = (&'a Path, usize)>,
) -> Result<(), FuseError> {
    let mut physical_entries = HashSet::new();
    let mut files = HashSet::new();
    let mut directories = HashSet::new();
    let mut remaining = COPY_LIMITS.total_bytes;
    for (path, bytes) in entries {
        if !files.insert(path) || directories.contains(path) {
            return Err(scan_error(
                "duplicate or overlapping Memory projection path",
            ));
        }
        if path.components().count() > COPY_LIMITS.depth
            || bytes as u64 > COPY_LIMITS.file_bytes
            || bytes as u64 > remaining
        {
            return Err(scan_error("Memory projection exceeds copy budget"));
        }
        remaining -= bytes as u64;
        for entry in path
            .ancestors()
            .take_while(|entry| !entry.as_os_str().is_empty())
        {
            if entry != path {
                if files.contains(entry) {
                    return Err(scan_error("file is an ancestor of another Memory path"));
                }
                directories.insert(entry);
            }
            physical_entries.insert(entry);
            if physical_entries.len() > COPY_LIMITS.entries {
                return Err(scan_error("Memory projection exceeds entry budget"));
            }
        }
    }
    Ok(())
}

/// True when this host can mount FUSE (`/dev/fuse` present + `fusermount` /
/// `fusermount3` on `PATH`). When false, use [`materialize`] / [`harvest`].
#[must_use]
pub fn fuse_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).exists()))
    };
    on_path("fusermount") || on_path("fusermount3")
}

/// Exact store heads observed while a copy realization was materialized. This is
/// transient mount state, not another resource/configuration model: it exists only
/// so teardown can reconcile agent edits without clobbering concurrent writers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopySnapshot {
    heads: BTreeMap<String, MemoryMaterializationHead>,
}

impl CopySnapshot {
    /// Number of memories copied from the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heads.len()
    }

    /// Whether no store heads were materialized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Secret-free CAS bases persisted with the current Sandbox handle. The
    /// path-ordered result has one canonical representation across providers.
    #[must_use]
    pub fn materialization_heads(&self) -> Vec<MemoryMaterializationHead> {
        self.heads.values().cloned().collect()
    }

    /// Rebuild the transient reconciliation cursor from the exact bases stored
    /// in a durable Sandbox handle. Missing, duplicate, or malformed identities
    /// fail closed instead of taking a fresh snapshot of current durable heads.
    pub fn from_materialization_heads(
        heads: &[MemoryMaterializationHead],
    ) -> Result<Self, FuseError> {
        let mut snapshot = Self::default();
        for head in heads {
            if head.path.trim().is_empty()
                || head.id.trim().is_empty()
                || head.content_sha256.trim().is_empty()
                || snapshot
                    .heads
                    .insert(head.path.clone(), head.clone())
                    .is_some()
            {
                return Err(FuseError::Internal(
                    "invalid or duplicate Memory materialization head".into(),
                ));
            }
        }
        Ok(snapshot)
    }
}

/// One local edit that could not be reconciled because the durable head changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestConflict {
    pub reason: &'static str,
    pub candidate: MemoryCopyConflict,
}

/// Copy reconciliation outcome. Conflicts preserve the durable head and are
/// explicit so the caller can surface/telemetry them rather than claiming success.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarvestReport {
    pub changed: usize,
    pub conflicts: Vec<HarvestConflict>,
}

/// Copy every memory in `store` out to `root/<path>` (the no-FUSE materialization).
/// Nested paths create their parent directories. Returns the exact observed heads
/// needed for conflict-safe teardown.
pub async fn materialize(
    fs: &dyn MemoryRepository,
    store: &str,
    root: &Path,
) -> Result<CopySnapshot, FuseError> {
    let mut snapshot = CopySnapshot::default();
    let memories = fs.snapshot_heads(store).await?;
    // Validate the ENTIRE logical set before the first filesystem mutation.
    // Store paths remain POSIX logical identities; host path syntax is never
    // accepted merely because this particular OS would allow it in a filename.
    let paths = memories
        .iter()
        .map(|memory| relative_memory_path(&memory.path))
        .collect::<Result<Vec<_>, _>>()?;
    validate_copy_entries(paths.iter().zip(&memories).map(|(path, memory)| {
        (
            path.as_path(),
            memory.content.as_deref().unwrap_or_default().len(),
        )
    }))?;
    std::fs::create_dir_all(root).map_err(scan_error)?;
    // Parent names are exclusive too: create_dir_all would silently coalesce
    // A/x and a/y on a case-insensitive filesystem, although both files are new.
    // Remember only parents created for this projection, never adopt an ambient
    // directory merely because the OS reports that it already exists.
    let mut parents = HashSet::new();
    for path in &paths {
        let ancestors: Vec<_> = path
            .ancestors()
            .skip(1)
            .filter(|parent| !parent.as_os_str().is_empty())
            .collect();
        for parent in ancestors.into_iter().rev() {
            if parents.insert(parent) {
                std::fs::create_dir(root.join(parent)).map_err(scan_error)?;
            }
        }
    }
    for (memory, relative) in memories.into_iter().zip(paths) {
        let dest = root.join(relative);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .and_then(|mut file| file.write_all(memory.content.unwrap_or_default().as_bytes()))
            .map_err(|e| FuseError::Internal(e.to_string()))?;
        let path = memory.path;
        snapshot.heads.insert(
            path.clone(),
            MemoryMaterializationHead {
                path,
                id: memory.id,
                version: memory.version,
                content_sha256: memory.content_sha256,
            },
        );
    }
    // Admission requires an exact round trip, including filesystem Unicode
    // normalization. Never publish a mount whose first no-edit harvest would
    // rename a logical head. Reuse the canonical bounded nofollow reader.
    let projected =
        awaken_sandbox_fs::read_tree_nofollow_with_limits(root, COPY_LIMITS).map_err(scan_error)?;
    if projected.files.len() != snapshot.heads.len() {
        return Err(scan_error("Memory projection changed the path set"));
    }
    for file in projected.files {
        let path = memory_path(&file.relative_path)?;
        let content = std::str::from_utf8(&file.bytes).map_err(scan_error)?;
        if snapshot
            .heads
            .get(&path)
            .is_none_or(|head| head.content_sha256 != sha256_hex(content))
        {
            return Err(scan_error(
                "Memory projection did not preserve exact path and bytes",
            ));
        }
    }
    Ok(snapshot)
}

/// Walk `root` and reconcile it against the heads captured by [`materialize`]. New
/// files are created, changed files update against the captured id+sha, unchanged
/// files are skipped, and removed files use an atomic delete-if-match. A concurrent
/// head always wins and is reported as a conflict; a path created after materialize
/// is never mistaken for a local deletion.
///
/// The deletion pass gives the no-FUSE copy tier the same durable outcome as the live
/// FUSE tier's `unlink` → `delete_by_path`: a memory materialized out to the copy dir
/// but no longer present there is deleted from the store, so "rm note.md" sticks on
/// both realization tiers.
pub async fn harvest(
    fs: &dyn MemoryRepository,
    store: &str,
    root: &Path,
    snapshot: &mut CopySnapshot,
) -> Result<HarvestReport, FuseError> {
    // Establish a complete readable projection BEFORE any Repository mutation.
    // A missing/unreadable tree is not an empty tree and cannot authorize delete.
    // The caller must quiesce writers; this is not an atomic filesystem snapshot
    // or a sandbox against an actively hostile TrustedHost process.
    let tree =
        awaken_sandbox_fs::read_tree_nofollow_with_limits(root, COPY_LIMITS).map_err(scan_error)?;
    for directory in &tree.directories {
        memory_path(&directory.relative_path)?;
    }
    let files = tree
        .files
        .into_iter()
        .map(|file| Ok((memory_path(&file.relative_path)?, file.bytes)))
        .collect::<Result<Vec<_>, FuseError>>()?;
    reconcile_files(fs, store, files, snapshot).await
}

/// Recover an already-captured projection without rematerializing sensitive
/// bytes on the host. Admission is all-or-nothing; Repository writes retain the
/// same per-file CAS/retry semantics as a live tree scan.
pub(crate) async fn harvest_recovered_files(
    fs: &dyn MemoryRepository,
    store: &str,
    files: &[(String, Vec<u8>)],
    snapshot: &mut CopySnapshot,
) -> Result<HarvestReport, FuseError> {
    let paths = files
        .iter()
        .map(|(relative, _)| {
            relative_memory_path(&format!("/{relative}")).map_err(|error| {
                FuseError::Internal(format!("unsafe recovered Memory copy path: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_copy_entries(
        paths
            .iter()
            .zip(files)
            .map(|(path, (_, bytes))| (path.as_path(), bytes.len())),
    )?;
    let mut contents = files
        .iter()
        .map(|(path, bytes)| (format!("/{path}"), bytes.clone()))
        .collect::<Vec<_>>();
    contents.sort_by(|a, b| a.0.cmp(&b.0));
    reconcile_files(fs, store, contents, snapshot).await
}

async fn reconcile_files(
    fs: &dyn MemoryRepository,
    store: &str,
    files: Vec<(String, Vec<u8>)>,
    snapshot: &mut CopySnapshot,
) -> Result<HarvestReport, FuseError> {
    // Memory stores text. A physically complete scan is not sufficient if any
    // candidate cannot be represented durably. Admit the WHOLE captured batch
    // before the first CAS or deletion; skipping a binary entry would let the
    // caller acknowledge reconciliation and dispose its only remaining bytes.
    // Live and recovered copies share this admission and mutation boundary.
    let files = files
        .into_iter()
        .map(|(path, bytes)| {
            String::from_utf8(bytes)
                .map(|content| (path, content))
                .map_err(|_| scan_error("non-UTF-8 Memory content cannot be reconciled"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let present: HashSet<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let mut report = HarvestReport::default();
    for (path, content) in &files {
        match snapshot.heads.get(path).cloned() {
            Some(base) => {
                if sha256_hex(content) == base.content_sha256 {
                    continue;
                }
                match fs
                    .update_materialized(store, path, &base.generation(), content)
                    .await
                {
                    Ok(updated) => {
                        snapshot.heads.insert(
                            path.clone(),
                            MemoryMaterializationHead {
                                path: path.clone(),
                                id: updated.id,
                                version: updated.version,
                                content_sha256: updated.content_sha256,
                            },
                        );
                        report.changed += 1;
                    }
                    Err(MemErr::Conflict { .. } | MemErr::NotFound(_)) => {
                        report.conflicts.push(HarvestConflict {
                            reason: "durable head changed",
                            candidate: MemoryCopyConflict {
                                path: path.clone(),
                                base: Some(MemoryCopyBase {
                                    id: base.id,
                                    version: base.version,
                                    content_sha256: base.content_sha256,
                                }),
                                content: Some(content.clone()),
                            },
                        });
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            None => match fs.create(store, path, content).await {
                Ok(created) => {
                    snapshot.heads.insert(
                        path.clone(),
                        MemoryMaterializationHead {
                            path: path.clone(),
                            id: created.id,
                            version: created.version,
                            content_sha256: created.content_sha256,
                        },
                    );
                    report.changed += 1;
                }
                Err(MemErr::PathConflict(_)) => {
                    let current = fs.get_by_path(store, path).await?;
                    if let Some(current) = current
                        && current.content_sha256 == sha256_hex(content)
                    {
                        snapshot.heads.insert(
                            path.clone(),
                            MemoryMaterializationHead {
                                path: path.clone(),
                                id: current.id,
                                version: current.version,
                                content_sha256: current.content_sha256,
                            },
                        );
                    } else {
                        report.conflicts.push(HarvestConflict {
                            reason: "path was concurrently created",
                            candidate: MemoryCopyConflict {
                                path: path.clone(),
                                base: None,
                                content: Some(content.clone()),
                            },
                        });
                    }
                }
                Err(error) => return Err(error.into()),
            },
        }
    }
    // Only heads present in the original snapshot can be local deletions. Anything
    // created in the durable store after materialization is outside this mount's
    // write set and must survive.
    let removed: Vec<_> = snapshot
        .heads
        .iter()
        .filter(|(path, _)| !present.contains(path.as_str()))
        .map(|(path, head)| (path.clone(), head.clone()))
        .collect();
    for (path, base) in removed {
        match fs.delete_if_match(store, &path, &base.generation()).await {
            Ok(deleted) => {
                snapshot.heads.remove(&path);
                report.changed += usize::from(deleted);
            }
            Err(MemErr::Conflict { .. }) => report.conflicts.push(HarvestConflict {
                candidate: MemoryCopyConflict {
                    path,
                    base: Some(MemoryCopyBase {
                        id: base.id,
                        version: base.version,
                        content_sha256: base.content_sha256,
                    }),
                    content: None,
                },
                reason: "removed path changed concurrently",
            }),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(report)
}

fn scan_error(error: impl std::fmt::Display) -> FuseError {
    FuseError::Internal(format!("Memory copy scan incomplete: {error}"))
}

/// One projection validator for initial materialization and recovered bytes.
pub(crate) fn relative_memory_path(logical: &str) -> Result<PathBuf, FuseError> {
    let relative = logical
        .strip_prefix('/')
        .ok_or_else(|| scan_error("Memory path is not absolute"))?;
    awaken_resource_contract::validate_path_len(logical).map_err(scan_error)?;
    if relative
        .split('/')
        .any(|name| !awaken_sandbox_fs::is_portable_component(name))
    {
        return Err(scan_error("Memory path cannot be projected portably"));
    }
    Ok(relative.split('/').collect())
}

/// Memory owns only its portable logical-name projection; directory walking,
/// nofollow opens and byte limits remain in the shared filesystem authority.
fn memory_path(relative: &Path) -> Result<String, FuseError> {
    let mut logical = String::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(scan_error("noncanonical relative Memory path"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| scan_error("non-UTF-8 filename"))?;
        if !awaken_sandbox_fs::is_portable_component(name) {
            return Err(scan_error("filename is not a portable component"));
        }
        logical.push('/');
        logical.push_str(name);
    }
    relative_memory_path(&logical)?;
    Ok(logical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::{MemErr, Memory, MemoryPurgeSummary, MemoryVersion};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use awaken_memory_store::VolatileMemoryRepository;

    #[test]
    fn copy_admission_uses_the_same_physical_budgets_as_harvest() {
        // Decision rules: depth/bytes <= limit and distinct ancestor count <=
        // limit => admit. Any excess => reject before filesystem writes. Shared
        // parents count once; empty bytes still consume metadata budget.
        for (depth, bytes, admitted) in [
            (128, COPY_LIMITS.file_bytes as usize, true),
            (129, 0, false),
            (1, COPY_LIMITS.file_bytes as usize + 1, false),
        ] {
            let path: PathBuf = std::iter::repeat_n("d", depth).collect();
            assert_eq!(
                validate_copy_entries([(path.as_path(), bytes)]).is_ok(),
                admitted
            );
        }
        for (shared, admitted) in [(true, true), (false, false)] {
            let paths: Vec<PathBuf> = (0..2000)
                .map(|i| {
                    if shared {
                        format!("a/b/{i}")
                    } else {
                        format!("{i}/a/b")
                    }
                    .into()
                })
                .collect();
            assert_eq!(
                validate_copy_entries(paths.iter().map(|p| (p.as_path(), 0))).is_ok(),
                admitted
            );
        }
        let paths: Vec<PathBuf> = (0..=awaken_resource_contract::MAX_MEMORIES_PER_STORE)
            .map(|i| PathBuf::from(format!("file-{i}")))
            .collect();
        assert!(
            validate_copy_entries(
                paths
                    .iter()
                    .map(|path| (path.as_path(), COPY_LIMITS.file_bytes as usize))
            )
            .is_err()
        );
    }

    fn temp(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("awaken-memcopy-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    struct SnapshotOnlyRepository {
        heads: Vec<Memory>,
    }

    #[async_trait::async_trait]
    impl MemoryRepository for SnapshotOnlyRepository {
        async fn preserve_copy_conflict(
            &self,
            _store: &str,
            _candidate: &awaken_resource_contract::MemoryCopyConflict,
        ) -> Result<(), MemErr> {
            unreachable!("snapshot test performs no conflict writes")
        }

        async fn list_copy_conflicts(
            &self,
            _store: &str,
        ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
            unreachable!("snapshot test performs no conflict reads")
        }

        async fn snapshot_heads(&self, _store: &str) -> Result<Vec<Memory>, MemErr> {
            Ok(self.heads.clone())
        }

        async fn list(
            &self,
            _store: &str,
            _prefix: &str,
        ) -> Result<Vec<awaken_memory_store::MemoryEntry>, MemErr> {
            panic!("the copy snapshot must not emulate an atomic snapshot with list")
        }

        async fn get_by_path(&self, _store: &str, _path: &str) -> Result<Option<Memory>, MemErr> {
            panic!("the copy snapshot must not emulate an atomic snapshot with per-path reads")
        }

        async fn create_as(
            &self,
            _store: &str,
            _path: &str,
            _content: &str,
            _actor: Option<&awaken_resource_contract::MemoryActor>,
        ) -> Result<Memory, MemErr> {
            unreachable!("snapshot test performs no writes")
        }

        async fn update_head_as(
            &self,
            _store: &str,
            _base: &awaken_resource_contract::MemoryGeneration,
            _content: &str,
            _target_path: Option<&str>,
            _actor: Option<&awaken_resource_contract::MemoryActor>,
        ) -> Result<Memory, MemErr> {
            unreachable!("snapshot test performs no writes")
        }

        async fn update_materialized(
            &self,
            _store: &str,
            _path: &str,
            _base: &awaken_resource_contract::MemoryGeneration,
            _content: &str,
        ) -> Result<Memory, MemErr> {
            Err(MemErr::Storage(
                "atomic materialized Memory update is unsupported".into(),
            ))
        }

        async fn rename(&self, _store: &str, _from: &str, _to: &str) -> Result<Memory, MemErr> {
            unreachable!("snapshot test performs no writes")
        }

        async fn delete_by_path_as(
            &self,
            _store: &str,
            _path: &str,
            _actor: Option<&awaken_resource_contract::MemoryActor>,
        ) -> Result<(), MemErr> {
            unreachable!("snapshot test performs no writes")
        }

        async fn delete_if_match_as(
            &self,
            _store: &str,
            _path: &str,
            _base: &awaken_resource_contract::MemoryGeneration,
            _actor: Option<&awaken_resource_contract::MemoryActor>,
        ) -> Result<bool, MemErr> {
            unreachable!("snapshot test performs no writes")
        }

        async fn list_versions(&self, _store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
            unreachable!("snapshot test performs no history reads")
        }

        async fn redact_version_as(
            &self,
            _store: &str,
            _version_id: &str,
            _actor: Option<&awaken_resource_contract::MemoryActor>,
        ) -> Result<Option<MemoryVersion>, MemErr> {
            unreachable!("snapshot test performs no history writes")
        }

        async fn purge_store(&self, _store: &str) -> Result<MemoryPurgeSummary, MemErr> {
            unreachable!("snapshot test performs no lifecycle writes")
        }
    }

    /// Cause/effect decision table for frozen copy inputs:
    /// | Rule | atomic heads result | legacy list/get availability | Effect |
    /// |---|---|---|---|
    /// | S1 | two path-ordered heads | panic if called | materialize both files and capture both CAS bases |
    /// | S2 | two path-ordered heads | panic if called | recovery snapshot captures both CAS bases without writing files |
    #[test]
    fn copy_inputs_use_the_one_atomic_snapshot_primitive() {
        let rt = rt();
        let heads = vec![
            Memory {
                id: "memory-a".into(),
                path: "/a.md".into(),
                content_sha256: awaken_memory_store::sha256_hex("a"),
                content_size: 1,
                version: 1,
                created_unix_nanos: 1,
                updated_unix_nanos: 1,
                content: Some("a".into()),
            },
            Memory {
                id: "memory-b".into(),
                path: "/nested/b.md".into(),
                content_sha256: awaken_memory_store::sha256_hex("b"),
                content_size: 1,
                version: 2,
                created_unix_nanos: 1,
                updated_unix_nanos: 2,
                content: Some("b".into()),
            },
        ];
        let fs = SnapshotOnlyRepository {
            heads: heads.clone(),
        };
        let dir = temp("atomic-snapshot");

        let materialized = rt.block_on(materialize(&fs, "s", &dir)).expect("S1");
        assert_eq!(materialized.len(), 2, "S1");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.md")).unwrap(),
            "a",
            "S1"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("nested/b.md")).unwrap(),
            "b",
            "S1"
        );

        let recovered =
            CopySnapshot::from_materialization_heads(&materialized.materialization_heads())
                .expect("S2");
        assert_eq!(recovered.len(), 2, "S2");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn unsupported_path_cas_never_falls_back_to_id_only_write() {
        // MP-U: adapter has no atomic location operation => error before any
        // get/update call. The existing snapshot-only double panics on either,
        // detecting a racy read-then-update fallback in the contract default.
        let fs = SnapshotOnlyRepository { heads: vec![] };
        assert!(
            matches!(fs.update_materialized("s", "/a", &awaken_resource_contract::MemoryGeneration::new("id", 1, "sha"), "edit").await,
            Err(MemErr::Storage(message)) if message.contains("unsupported"))
        );
    }

    #[test]
    fn fuse_available_is_a_bool() {
        // Just exercise the probe; the value depends on the host.
        let _ = fuse_available();
    }

    #[test]
    fn materialize_writes_files_including_nested_paths() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        rt.block_on(fs.create("s", "/root.md", "top")).unwrap();
        rt.block_on(fs.create("s", "/notes/deep/a.md", "nested"))
            .unwrap();
        let dir = temp("mat");

        let snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(!snapshot.is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("root.md")).unwrap(), "top");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes/deep/a.md")).unwrap(),
            "nested"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_creates_new_updates_changed_and_skips_unchanged() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        // Seed one memory; a round-trip will modify it and add another.
        let seed = rt.block_on(fs.create("s", "/keep.md", "v1")).unwrap();
        let dir = temp("harv");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        // Agent edits /keep.md, adds /new.md, and leaves an unchanged copy behind.
        std::fs::write(dir.join("keep.md"), "v2").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new.md"), "fresh").unwrap();

        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(
            report.changed, 2,
            "one updated + one created; the unchanged file is a no-op"
        );
        assert!(report.conflicts.is_empty());

        let keep = rt
            .block_on(fs.get_by_path("s", "/keep.md"))
            .unwrap()
            .unwrap();
        assert_eq!(keep.content.as_deref(), Some("v2"));
        assert!(
            keep.version > seed.version,
            "a changed file bumps the version"
        );
        assert_eq!(
            rt.block_on(fs.get_by_path("s", "/sub/new.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("fresh"),
        );

        // A second harvest with no edits changes nothing (idempotent).
        assert_eq!(
            rt.block_on(harvest(&*fs, "s", &dir, &mut snapshot))
                .unwrap()
                .changed,
            0
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_rejects_non_utf8_before_any_mutation() {
        // U1 valid create + non-UTF-8 create => error, no changed heads or
        // snapshot, exact binary source retained. The integration matrix covers
        // edits, deletions, recovery, ordering, read-only and durable retry.
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let dir = temp("harv-bin");
        let mut snapshot = CopySnapshot::default();
        std::fs::write(dir.join("ok.md"), "text").unwrap();
        std::fs::write(dir.join("blob.bin"), [0xFF, 0xFE, 0x00]).unwrap();

        let error = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap_err();
        assert!(error.to_string().contains("non-UTF-8 Memory content"));
        assert!(rt.block_on(fs.snapshot_heads("s")).unwrap().is_empty());
        assert!(snapshot.is_empty());
        assert_eq!(
            std::fs::read(dir.join("blob.bin")).unwrap(),
            [0xFF, 0xFE, 0x00]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_deletes_a_removed_file_matching_fuse_unlink() {
        // The copy/harvest fallback deletes a memory whose file the agent removed from
        // the materialized copy dir, giving the same durable outcome as the live FUSE
        // path (`unlink` → `delete_by_path`). So "rm note.md" sticks identically across
        // both realization tiers (FUSE and the no-FUSE bwrap / CI / macOS copy tier).
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        rt.block_on(fs.create("s", "/keep.md", "x")).unwrap();
        rt.block_on(fs.create("s", "/gone.md", "y")).unwrap();
        let dir = temp("del-parity");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        // The agent removes gone.md from the copy dir; the turn ends → harvest.
        std::fs::remove_file(dir.join("gone.md")).unwrap();
        assert_eq!(
            rt.block_on(harvest(&*fs, "s", &dir, &mut snapshot))
                .unwrap()
                .changed,
            1,
            "harvest folds the removed file back as one deletion"
        );

        // Parity with FUSE unlink: the removed file is gone from the store…
        assert!(
            rt.block_on(fs.get_by_path("s", "/gone.md"))
                .unwrap()
                .is_none(),
            "copy harvest deletes a removed file, matching FUSE unlink"
        );
        // …and an untouched memory is left intact.
        assert!(
            rt.block_on(fs.get_by_path("s", "/keep.md"))
                .unwrap()
                .is_some(),
            "an unremoved memory survives the harvest"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_on_a_missing_root_fails_even_without_captured_heads() {
        // M0: missing root + empty base => error, not successful empty harvest.
        // Nonempty bases and retry after restoration belong to copy_lifecycle.
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let dir = temp("missing");
        let missing = dir.join("absent");
        let mut snapshot = CopySnapshot::default();
        assert!(
            rt.block_on(harvest(&*fs, "s", &missing, &mut snapshot))
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_update_wins_over_a_stale_local_edit() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let original = rt.block_on(fs.create("s", "/note.md", "v1")).unwrap();
        let dir = temp("concurrent-update");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        std::fs::write(dir.join("note.md"), "local-v2").unwrap();
        rt.block_on(fs.update("s", &original.generation(), "remote-v2"))
            .unwrap();

        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(report.changed, 0);
        assert_eq!(report.conflicts[0].candidate.path, "/note.md");
        assert_eq!(
            rt.block_on(fs.get_by_path("s", "/note.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("remote-v2"),
            "copy reconciliation never clobbers the concurrent durable head"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_create_is_not_deleted_as_locally_absent() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        rt.block_on(fs.create("s", "/before.md", "before")).unwrap();
        let dir = temp("concurrent-create");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        rt.block_on(fs.create("s", "/remote.md", "remote")).unwrap();
        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(report.changed, 0);
        assert!(report.conflicts.is_empty());
        assert!(
            rt.block_on(fs.get_by_path("s", "/remote.md"))
                .unwrap()
                .is_some(),
            "a path created after materialization is outside the local delete set"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_update_blocks_a_stale_local_delete() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let original = rt.block_on(fs.create("s", "/note.md", "v1")).unwrap();
        let dir = temp("concurrent-delete");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        std::fs::remove_file(dir.join("note.md")).unwrap();
        rt.block_on(fs.update("s", &original.generation(), "remote-v2"))
            .unwrap();
        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(report.changed, 0);
        assert_eq!(report.conflicts[0].candidate.path, "/note.md");
        assert_eq!(
            rt.block_on(fs.get_by_path("s", "/note.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("remote-v2")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
