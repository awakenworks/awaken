//! Copy-in / harvest fallback (ADR-0053 D6) for hosts without FUSE.
//!
//! A write-through FUSE mount is the live path, but it needs `/dev/fuse` — absent on
//! macOS, in CI, and in unprivileged containers. There, the same path-addressed
//! [`MemoryRepository`] store is materialized the old way: [`materialize`] copies every
//! memory out to plain files the agent reads/writes, and [`harvest`] walks those
//! files back into the store after a turn (create new, CAS-update changed, skip
//! unchanged). [`fuse_available`] is the capability check a provider gates on.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use awaken_memory_store::{MemErr, MemoryRepository, sha256_hex};

use crate::FuseError;

/// True when this host can mount FUSE (`/dev/fuse` present + `fusermount` /
/// `fusermount3` on `PATH`). When false, use [`materialize`] / [`harvest`].
#[must_use]
pub fn fuse_available() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).exists()))
    };
    on_path("fusermount") || on_path("fusermount3")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyHead {
    id: String,
    sha256: String,
}

/// Exact store heads observed while a copy realization was materialized. This is
/// transient mount state, not another resource/configuration model: it exists only
/// so teardown can reconcile agent edits without clobbering concurrent writers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopySnapshot {
    heads: BTreeMap<String, CopyHead>,
}

impl CopySnapshot {
    /// Number of memories copied from the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heads.len()
    }

    /// Whether no memories were copied from the store.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }
}

/// One local edit that could not be reconciled because the durable head changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestConflict {
    pub path: String,
    pub reason: &'static str,
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
    let entries = fs.list(store, "/").await?;
    let mut snapshot = CopySnapshot::default();
    for entry in &entries {
        let Some(memory) = fs.get_by_path(store, &entry.path).await? else {
            continue;
        };
        let dest = root.join(entry.path.trim_start_matches('/'));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FuseError::Internal(e.to_string()))?;
        }
        std::fs::write(&dest, memory.content.unwrap_or_default())
            .map_err(|e| FuseError::Internal(e.to_string()))?;
        snapshot.heads.insert(
            entry.path.clone(),
            CopyHead {
                id: memory.id,
                sha256: memory.content_sha256,
            },
        );
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
    let mut files = Vec::new();
    collect_files(root, root, &mut files);
    let present: HashSet<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let mut report = HarvestReport::default();
    for (path, host_path) in &files {
        let Ok(content) = std::fs::read_to_string(host_path) else {
            continue; // non-UTF-8 memory bytes are not our model — skip
        };
        match snapshot.heads.get(path).cloned() {
            Some(base) => {
                if sha256_hex(&content) == base.sha256 {
                    continue;
                }
                match fs.update(store, &base.id, &content, &base.sha256).await {
                    Ok(updated) => {
                        snapshot.heads.insert(
                            path.clone(),
                            CopyHead {
                                id: updated.id,
                                sha256: updated.content_sha256,
                            },
                        );
                        report.changed += 1;
                    }
                    Err(MemErr::Conflict { .. } | MemErr::NotFound(_)) => {
                        report.conflicts.push(HarvestConflict {
                            path: path.clone(),
                            reason: "durable head changed",
                        });
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            None => match fs.create(store, path, &content).await {
                Ok(created) => {
                    snapshot.heads.insert(
                        path.clone(),
                        CopyHead {
                            id: created.id,
                            sha256: created.content_sha256,
                        },
                    );
                    report.changed += 1;
                }
                Err(MemErr::PathConflict(_)) => {
                    let current = fs.get_by_path(store, path).await?;
                    if let Some(current) = current
                        && current.content_sha256 == sha256_hex(&content)
                    {
                        snapshot.heads.insert(
                            path.clone(),
                            CopyHead {
                                id: current.id,
                                sha256: current.content_sha256,
                            },
                        );
                    } else {
                        report.conflicts.push(HarvestConflict {
                            path: path.clone(),
                            reason: "path was concurrently created",
                        });
                    }
                }
                Err(MemErr::InvalidPath(_)) => report.conflicts.push(HarvestConflict {
                    path: path.clone(),
                    reason: "invalid memory path",
                }),
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
        match fs
            .delete_if_match(store, &path, &base.id, &base.sha256)
            .await
        {
            Ok(deleted) => {
                snapshot.heads.remove(&path);
                report.changed += usize::from(deleted);
            }
            Err(MemErr::Conflict { .. }) => report.conflicts.push(HarvestConflict {
                path,
                reason: "removed path changed concurrently",
            }),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(report)
}

/// Recursively collect `(store_path, host_path)` for every file under `base`, where
/// `store_path` is the memory path (`/` + the path relative to `root`).
fn collect_files(root: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = rel.to_string_lossy().replace('\\', "/");
            out.push((format!("/{rel}"), path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use awaken_memory_store::VolatileMemoryRepository;

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
    fn harvest_skips_a_non_utf8_file_rather_than_failing() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let dir = temp("harv-bin");
        let mut snapshot = CopySnapshot::default();
        std::fs::write(dir.join("ok.md"), "text").unwrap();
        std::fs::write(dir.join("blob.bin"), [0xFF, 0xFE, 0x00]).unwrap();

        // The UTF-8 file is folded in; the binary one is skipped, not fatal.
        assert_eq!(
            rt.block_on(harvest(&*fs, "s", &dir, &mut snapshot))
                .unwrap()
                .changed,
            1
        );
        assert!(
            rt.block_on(fs.get_by_path("s", "/blob.bin"))
                .unwrap()
                .is_none()
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
    fn harvest_on_a_missing_root_is_empty() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let missing = std::env::temp_dir().join("awaken-memcopy-does-not-exist-xyz");
        let mut snapshot = CopySnapshot::default();
        assert_eq!(
            rt.block_on(harvest(&*fs, "s", &missing, &mut snapshot))
                .unwrap()
                .changed,
            0
        );
    }

    #[test]
    fn concurrent_update_wins_over_a_stale_local_edit() {
        let rt = rt();
        let fs = Arc::new(VolatileMemoryRepository::new());
        let original = rt.block_on(fs.create("s", "/note.md", "v1")).unwrap();
        let dir = temp("concurrent-update");
        let mut snapshot = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        std::fs::write(dir.join("note.md"), "local-v2").unwrap();
        rt.block_on(fs.update("s", &original.id, "remote-v2", &original.content_sha256))
            .unwrap();

        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(report.changed, 0);
        assert_eq!(report.conflicts[0].path, "/note.md");
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
        rt.block_on(fs.update("s", &original.id, "remote-v2", &original.content_sha256))
            .unwrap();
        let report = rt
            .block_on(harvest(&*fs, "s", &dir, &mut snapshot))
            .unwrap();
        assert_eq!(report.changed, 0);
        assert_eq!(report.conflicts[0].path, "/note.md");
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
