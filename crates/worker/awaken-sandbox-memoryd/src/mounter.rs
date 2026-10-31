// @formal evidence memory.cas_has_one_winner_per_generation
// @formal evidence memory.idempotent_write_does_not_advance_generation
// @formal evidence memory.stale_cas_never_clobbers_live_content
// @formal evidence memory.stale_conditional_delete_never_removes_live_head
//! The provider-facing memory-store realization (ADR-0053, item 1).
//!
//! [`MemoryStoreMounter`] implements the neutral [`MemoryMounter`] port a sandbox
//! provider calls to realize a `MountSource::MemoryStore`. It exposes the store as a
//! **live FUSE mount** where the kernel supports it ([`fuse_available`]), and
//! otherwise falls back to a **copy** that is harvested back to the store on teardown
//! (ADR-0053 D6). Both paths write through the same durable Memory role views, wrapped in
//! the Resources-owned decorator over one shared bus so a write in one sandbox drops the
//! path from every other sandbox's FUSE cache (the D5 coherence model, applied
//! per-sandbox rather than via a single shared mount — no bind/namespace splice
//! needed, so it works on the unprivileged Workdir tier).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::copy;
use awaken_provisioning_contract::{
    MemoryMount, MemoryMountOutcome, MemoryMounter, MountAccess, Realization, SandboxError,
};
use awaken_resource_contract::LocalInvalidator;
use awaken_resource_contract::{
    MemoryCopyConflictPreservation, MemoryCopyWriteback, MemoryMountSource, MemoryQueries,
    MemorySnapshotSource,
};

#[derive(Clone)]
struct CopyMountContent {
    queries: Arc<dyn MemoryQueries>,
    snapshots: Arc<dyn MemorySnapshotSource>,
    writeback: Arc<dyn MemoryCopyWriteback>,
    conflicts: Arc<dyn MemoryCopyConflictPreservation>,
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
struct FuseProjection {
    filesystem: Arc<dyn awaken_resource_contract::MemoryFilesystem>,
    bus: Arc<LocalInvalidator>,
    clock: Arc<dyn awaken_time::WallClock>,
    #[cfg(test)]
    mounted_cache: std::sync::Mutex<Option<crate::fuse::MountedCacheWitness>>,
}

/// Realizes copies or live mounts from explicitly selected narrow capabilities.
/// All writes already pass through the Resources-owned decorator. This component
/// never constructs another bus or wraps a raw backend.
pub struct MemoryStoreMounter {
    copy: CopyMountContent,
    #[cfg(all(feature = "fuse", target_os = "linux"))]
    fuse: Option<FuseProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryMounterConstructionError {
    #[error("Memory mount bus does not belong to the selected write decorator")]
    InvalidationBusMismatch,
}

impl MemoryStoreMounter {
    #[cfg(all(test, feature = "fuse", target_os = "linux"))]
    pub(crate) fn shares_invalidator_for_test(&self, expected: &Arc<LocalInvalidator>) -> bool {
        Arc::ptr_eq(
            &self.fuse.as_ref().expect("FUSE mode selected").bus,
            expected,
        )
    }

    #[cfg(all(test, feature = "fuse", target_os = "linux"))]
    pub(crate) fn invalidations_for_test(
        &self,
    ) -> tokio::sync::broadcast::Receiver<awaken_resource_contract::Invalidation> {
        self.fuse
            .as_ref()
            .expect("FUSE mode selected")
            .bus
            .subscribe()
    }

    #[cfg(all(test, feature = "fuse", target_os = "linux"))]
    pub(crate) fn mounted_cache_for_test(&self) -> Option<crate::fuse::MountedCacheWitness> {
        self.fuse
            .as_ref()?
            .mounted_cache
            .lock()
            .expect("mount witness mutex is healthy")
            .clone()
    }

    /// Select CopyOnly capabilities from one actual backend/client instance.
    #[must_use]
    pub fn copy_only<T>(backend: Arc<T>) -> Self
    where
        T: MemoryQueries
            + MemorySnapshotSource
            + MemoryCopyWriteback
            + MemoryCopyConflictPreservation
            + 'static,
    {
        Self {
            copy: CopyMountContent {
                queries: backend.clone(),
                snapshots: backend.clone(),
                writeback: backend.clone(),
                conflicts: backend,
            },
            #[cfg(all(feature = "fuse", target_os = "linux"))]
            fuse: None,
        }
    }

    /// Select CopyOnly from the root's already-controlled source, retaining no FS capability.
    #[must_use]
    pub fn copy_only_from_source(source: MemoryMountSource) -> Self {
        Self {
            copy: CopyMountContent {
                queries: source.queries(),
                snapshots: source.snapshots(),
                writeback: source.writeback(),
                conflicts: source.conflict_preservation(),
            },
            #[cfg(all(feature = "fuse", target_os = "linux"))]
            fuse: None,
        }
    }

    /// Reject a second bus before retaining a projection or starting any listener/mount.
    pub fn fuse_preferred(
        source: MemoryMountSource,
        bus: Arc<LocalInvalidator>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, MemoryMounterConstructionError> {
        let identity: Arc<dyn awaken_resource_contract::MemoryInvalidator> = bus.clone();
        if !source.uses_invalidator(&identity) {
            return Err(MemoryMounterConstructionError::InvalidationBusMismatch);
        }
        let copy = CopyMountContent {
            queries: source.queries(),
            snapshots: source.snapshots(),
            writeback: source.writeback(),
            conflicts: source.conflict_preservation(),
        };
        #[cfg(not(all(feature = "fuse", target_os = "linux")))]
        let _ = (source, bus, clock);
        Ok(Self {
            copy,
            #[cfg(all(feature = "fuse", target_os = "linux"))]
            fuse: Some(FuseProjection {
                filesystem: source.filesystem(),
                bus,
                clock,
                #[cfg(test)]
                mounted_cache: std::sync::Mutex::new(None),
            }),
        })
    }
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
fn is_mountpoint(path: &Path) -> bool {
    let expected = path.to_string_lossy();
    std::fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .is_some_and(|mounts| {
            mounts.lines().any(|line| {
                line.split_whitespace()
                    .nth(4)
                    .map(|value| {
                        value
                            .replace("\\040", " ")
                            .replace("\\011", "\t")
                            .replace("\\134", "\\")
                    })
                    .is_some_and(|mountpoint| mountpoint == expected)
            })
        })
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
fn detach_fuse(path: &Path) {
    'detach: for args in [["-u", ""], ["-u", "-z"]] {
        for command in ["fusermount3", "fusermount"] {
            let mut process = std::process::Command::new(command);
            process.arg(args[0]);
            if !args[1].is_empty() {
                process.arg(args[1]);
            }
            if process
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                break 'detach;
            }
        }
    }
}

/// Remove a prior realization without following symlinks. A hard-killed process
/// can leave a disconnected FUSE mount at the durable Session path; detach that
/// resource-plane projection and retry removal before mounting the same binding.
fn clear_projection(host_path: &Path) -> Result<(), SandboxError> {
    // A disconnected FUSE mount returns ENOTCONN even for metadata, so detect and
    // detach it before the ordinary no-path fast path below.
    #[cfg(all(feature = "fuse", target_os = "linux"))]
    if is_mountpoint(host_path) {
        detach_fuse(host_path);
    }
    let Ok(metadata) = std::fs::symlink_metadata(host_path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return std::fs::remove_file(host_path)
            .map_err(|error| SandboxError::backend("clear_projection", error));
    }
    if std::fs::remove_dir_all(host_path).is_ok() && matches!(host_path.try_exists(), Ok(false)) {
        return Ok(());
    }

    #[cfg(all(feature = "fuse", target_os = "linux"))]
    detach_fuse(host_path);
    // A lazy detach is asynchronous in the kernel. Bound the wait so a recovered
    // Session does not race `spawn_mount2` against its dead predecessor.
    let mut last_error = None;
    for _ in 0..20 {
        match std::fs::remove_dir_all(host_path) {
            Ok(()) if matches!(host_path.try_exists(), Ok(false)) => return Ok(()),
            Ok(()) => {}
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(SandboxError::backend(
        "clear_projection",
        format!(
            "clear prior Memory projection `{}`: {}",
            host_path.display(),
            last_error.map_or_else(
                || "mountpoint remained after removal".into(),
                |error| error.to_string()
            )
        ),
    ))
}

#[async_trait::async_trait]
impl MemoryMounter for MemoryStoreMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> MemoryMountOutcome {
        let result: Result<Box<dyn MemoryMount>, SandboxError> = async {
            // Every realization is a fresh projection. In particular, a deleted
            // memory from the durable store must not reappear from stale copy bytes
            // left by an evicted Session context. Never follow a replaced symlink.
            clear_projection(host_path)?;
            std::fs::create_dir_all(host_path).map_err(|error| {
                SandboxError::backend(
                    "mount",
                    format!(
                        "create Memory projection `{}`: {error}",
                        host_path.display()
                    ),
                )
            })?;

            #[cfg(all(feature = "fuse", target_os = "linux"))]
            if let Some(projection) = self.fuse.as_ref()
                && copy::fuse_available()
            {
                let handle = crate::fuse::spawn_mount_with_invalidations(
                    projection.filesystem.clone(),
                    store_id.to_string(),
                    host_path.to_path_buf(),
                    projection.bus.subscribe(),
                    projection.clock.as_ref(),
                )
                .map_err(|error| {
                    SandboxError::backend(
                        "mount",
                        format!(
                            "mount MemoryStore `{store_id}` at `{}`: {error}",
                            host_path.display()
                        ),
                    )
                })?;
                #[cfg(test)]
                {
                    *projection
                        .mounted_cache
                        .lock()
                        .expect("mount witness mutex is healthy") =
                        Some(handle.cache_witness_for_test());
                }
                return Ok(Box::new(FuseMount {
                    handle: Arc::new(awaken_sync::InvariantLock::new(handle)),
                }) as Box<dyn MemoryMount>);
            }

            // No FUSE (macOS / CI / unprivileged container): copy the store out now and
            // harvest a writable mount back on teardown.
            let root = awaken_fs_durability::ProjectionRoot::capture(host_path)
                .map_err(|error| SandboxError::backend("mount", error))?;
            let snapshot = copy::materialize(self.copy.snapshots.as_ref(), store_id, host_path)
                .await
                .map_err(SandboxError::from)?;
            let materialization_heads = snapshot.materialization_heads();
            Ok(Box::new(CopyMount {
                root,
                copy: self.copy.clone(),
                store_id: store_id.to_string(),
                host_path: host_path.to_path_buf(),
                writable: access == MountAccess::ReadWrite,
                materialization_heads,
                snapshot: tokio::sync::Mutex::new(snapshot),
            }) as Box<dyn MemoryMount>)
        }
        .await;
        match result {
            Ok(mount) => MemoryMountOutcome::Mounted(mount),
            // Every fallible step follows `clear_projection` or may itself have
            // partially changed the stable host path. Retain recovery rather
            // than asserting that an ordinary error proves absence of effect.
            Err(error) => MemoryMountOutcome::RecoveryRequired(error),
        }
    }

    async fn recover_unknown_mount(
        &self,
        _store_id: &str,
        host_path: &Path,
        _access: MountAccess,
    ) -> Result<(), SandboxError> {
        clear_projection(host_path)
    }

    async fn reconcile_recovered_copy(
        &self,
        operation_reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: MountAccess,
    ) -> Result<(), SandboxError> {
        evidence.validate()?;
        if access != MountAccess::ReadWrite {
            return Ok(());
        }
        let mut snapshot = copy::CopySnapshot::from_materialization_heads(&evidence.heads)
            .map_err(SandboxError::from)?;
        let report = copy::harvest_recovered_files(
            self.copy.queries.as_ref(),
            self.copy.writeback.as_ref(),
            operation_reference,
            files,
            &mut snapshot,
        )
        .await
        .map_err(SandboxError::from)?;
        // Terminal replay alone may retire a copy after preserving every losing
        // proposal in its original Memory aggregate. Live harvest still reports
        // conflicts. An unsupported, denied or failed durable write remains an
        // error; neither head overwrite nor a transient report permits disposal.
        for conflict in report.conflicts {
            self.copy
                .conflicts
                .preserve_copy_conflict(operation_reference, &conflict.candidate)
                .await
                .map_err(SandboxError::from)?;
        }
        Ok(())
    }
}

/// A live FUSE mount; teardown unmounts (draining open fds).
#[cfg(all(feature = "fuse", target_os = "linux"))]
struct FuseMount {
    handle: Arc<awaken_sync::InvariantLock<crate::fuse::MemoryMountHandle>>,
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
#[async_trait::async_trait]
impl MemoryMount for FuseMount {
    fn realization(&self) -> Realization {
        Realization::Fuse
    }

    async fn teardown(&self) -> Result<(), SandboxError> {
        // `unmount` blocks briefly (drains fds, joins the session); keep it off the
        // async worker. The handle stays in the guard so a task failure remains
        // retryable; `MemoryMountHandle::unmount` is itself idempotent.
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || {
            handle
                .lock()
                .map_err(|_| {
                    SandboxError::backend("teardown", "Memory FUSE handle mutex is poisoned")
                })?
                .unmount();
            Ok(())
        })
        .await
        .map_err(|error| {
            SandboxError::backend("teardown", format!("join Memory FUSE teardown: {error}"))
        })?
    }
}

/// A materialized copy; teardown harvests a writable copy back to the store.
struct CopyMount {
    root: awaken_fs_durability::ProjectionRoot,
    copy: CopyMountContent,
    store_id: String,
    host_path: PathBuf,
    writable: bool,
    materialization_heads: Vec<awaken_provisioning_contract::MemoryMaterializationHead>,
    snapshot: tokio::sync::Mutex<copy::CopySnapshot>,
}

#[async_trait::async_trait]
impl MemoryMount for CopyMount {
    fn realization(&self) -> Realization {
        Realization::Copy
    }

    fn materialization_heads(
        &self,
    ) -> Option<Vec<awaken_provisioning_contract::MemoryMaterializationHead>> {
        Some(self.materialization_heads.clone())
    }

    async fn teardown(&self) -> Result<(), SandboxError> {
        if self.writable {
            let mut snapshot = self.snapshot.lock().await;
            self.root
                .verify(&self.host_path)
                .map_err(|error| SandboxError::backend("teardown", error))?;
            let report = copy::harvest_with_roles(
                self.copy.queries.as_ref(),
                self.copy.writeback.as_ref(),
                &self.store_id,
                &self.host_path,
                &mut snapshot,
            )
            .await
            .map_err(SandboxError::from)?;
            if !report.conflicts.is_empty() {
                return Err(SandboxError::conflict(format!(
                    "Memory copy harvest preserved {} concurrent durable head conflict(s)",
                    report.conflicts.len()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::VolatileMemoryRepository;
    use awaken_resource_contract::MemoryCopyConflictQueries as _;
    use awaken_resource_contract::MemoryWriter as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    trait MemoryMountOutcomeTestExt {
        fn mounted(self) -> Box<dyn MemoryMount>;
    }

    impl MemoryMountOutcomeTestExt for MemoryMountOutcome {
        fn mounted(self) -> Box<dyn MemoryMount> {
            match self {
                MemoryMountOutcome::Mounted(mount) => mount,
                MemoryMountOutcome::NoEffect(error)
                | MemoryMountOutcome::RecoveryRequired(error) => {
                    panic!("expected mounted Memory participant: {error}")
                }
            }
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("awaken-mounter-{tag}-{}-{n}", std::process::id()))
    }

    #[tokio::test]
    async fn copy_fallback_materializes_then_harvests_a_writable_edit() {
        // Force the copy path by pointing at a store with content and a host dir; on a
        // host with FUSE the mounter would pick FUSE, so we drive the copy guard
        // directly to assert the harvest-on-teardown contract deterministically.
        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir = temp("copy");

        let snapshot = copy::materialize(mounter.copy.snapshots.as_ref(), "s", &dir)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");

        // Agent edits the file; the writable copy guard harvests it back.
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        let materialization_heads = snapshot.materialization_heads();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            root: awaken_fs_durability::ProjectionRoot::capture(&dir).unwrap(),
            copy: mounter.copy.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: true,
            materialization_heads,
            snapshot: tokio::sync::Mutex::new(snapshot),
        });
        assert_eq!(guard.realization(), Realization::Copy);
        guard.teardown().await.unwrap();

        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("v2"),
            "harvest wrote the edit back to the durable store"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn recovered_copy_reconciles_surviving_files_without_a_live_guard() {
        // Recovered-copy cause/effect rule M1: exact persisted base A + surviving
        // edits B authorizes the same CAS harvest as a live CopyMount; no fresh
        // snapshot may replace A at recovery time, and successful writes become
        // the only durable heads before the Sandbox can be removed.
        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        let original = durable.create("s", "/note.md", "before").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let evidence = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
            "s",
            "/workspace/memory",
            vec![awaken_provisioning_contract::MemoryMaterializationHead {
                path: original.path,
                id: original.id,
                version: original.version,
                content_sha256: original.content_sha256,
            }],
        )
        .unwrap();

        mounter
            .reconcile_recovered_copy(
                "s",
                &evidence,
                &[
                    ("note.md".into(), b"after".to_vec()),
                    ("nested/new.md".into(), b"new".to_vec()),
                ],
                MountAccess::ReadWrite,
            )
            .await
            .unwrap();

        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("after")
        );
        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/nested/new.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("new")
        );
        // M2 stale recovery evidence + conflicting edit => durable candidate,
        // never overwrite the committed head. M3 read-only => no writes even
        // with stale evidence. M4 traversal => reject before any store effects.
        let stale = [("note.md".into(), b"stale overwrite".to_vec())];
        mounter
            .reconcile_recovered_copy("s", &evidence, &stale, MountAccess::ReadWrite)
            .await
            .unwrap();
        let retained = durable.list_copy_conflicts("s").await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].content.as_deref(), Some("stale overwrite"));
        mounter
            .reconcile_recovered_copy("s", &evidence, &stale, MountAccess::ReadOnly)
            .await
            .unwrap();
        assert_eq!(durable.list_copy_conflicts("s").await.unwrap(), retained);
        let error = mounter
            .reconcile_recovered_copy(
                "s",
                &evidence,
                &[("../escape.md".into(), b"escape".to_vec())],
                MountAccess::ReadWrite,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsafe recovered Memory copy path")
        );
        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("after")
        );
        assert!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/escape.md")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_read_only_copy_mount_does_not_harvest() {
        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir = temp("ro");
        let snapshot = copy::materialize(mounter.copy.snapshots.as_ref(), "s", &dir)
            .await
            .unwrap();

        // A read-only edit on disk must NOT propagate back.
        std::fs::write(dir.join("note.md"), "tampered").unwrap();
        let materialization_heads = snapshot.materialization_heads();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            root: awaken_fs_durability::ProjectionRoot::capture(&dir).unwrap(),
            copy: mounter.copy.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: false,
            materialization_heads,
            snapshot: tokio::sync::Mutex::new(snapshot),
        });
        guard.teardown().await.unwrap();

        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("v1"),
            "a read-only mount leaves the store untouched"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn distinct_stores_are_content_isolated_not_just_distinct_paths() {
        use awaken_provisioning_contract::{MemoryMounter, MountAccess};
        // Beyond the coordinator's "distinct stores → distinct mountpoints": a memory
        // written to store `alpha` must be INVISIBLE through store `beta`, even though
        // both realize over the same shared durable fs. `store_id` is a hard content
        // boundary — a peer store cannot see, or materialize, another's bytes.
        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        durable
            .create("alpha", "/secret.md", "alpha-only")
            .await
            .unwrap();
        durable
            .create("beta", "/note.md", "beta-only")
            .await
            .unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir_a = temp("iso-a");
        let dir_b = temp("iso-b");

        let ga = mounter
            .mount("alpha", &dir_a, MountAccess::ReadOnly)
            .await
            .mounted();
        let gb = mounter
            .mount("beta", &dir_b, MountAccess::ReadOnly)
            .await
            .mounted();

        // alpha's file materializes only under alpha's mount, never beta's (content
        // invisibility, not merely a different mountpoint).
        assert_eq!(
            std::fs::read_to_string(dir_a.join("secret.md")).unwrap(),
            "alpha-only"
        );
        assert!(
            !dir_b.join("secret.md").exists(),
            "alpha's content is invisible inside beta's mount"
        );
        assert!(
            !dir_a.join("note.md").exists(),
            "beta's content is invisible inside alpha's mount"
        );
        // The store read path is scoped too: neither store resolves the other's path.
        assert!(
            MemoryQueries::get_by_path(durable.as_ref(), "beta", "/secret.md")
                .await
                .unwrap()
                .is_none(),
            "beta cannot read alpha's memory"
        );
        assert!(
            MemoryQueries::get_by_path(durable.as_ref(), "alpha", "/note.md")
                .await
                .unwrap()
                .is_none(),
            "alpha cannot read beta's memory"
        );

        ga.teardown().await.unwrap();
        gb.teardown().await.unwrap();
        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }

    #[tokio::test]
    async fn copy_only_mount_realizes_copy_and_harvests_through_the_public_api() {
        use awaken_provisioning_contract::{MemoryMounter, MountAccess};
        // `copy_only` forces the no-FUSE path regardless of /dev/fuse on this host, so
        // this deterministically exercises the PUBLIC `mount()` → copy branch (not the
        // CopyMount constructor the other tests drive) — the exact path the
        // namespace/no-FUSE tier uses.
        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir = temp("copyonly");

        let guard = mounter
            .mount("s", &dir, MountAccess::ReadWrite)
            .await
            .mounted();
        assert_eq!(guard.realization(), Realization::Copy);
        // The store was materialized into the host dir (no FUSE).
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");

        // An edit is harvested back on teardown.
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        guard.teardown().await.unwrap();
        assert_eq!(
            MemoryQueries::get_by_path(durable.as_ref(), "s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("v2"),
            "copy_only mount harvested the edit back"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remount_starts_from_store_truth_and_drops_stale_projection_bytes() {
        use awaken_provisioning_contract::{MemoryMounter, MountAccess};

        let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        durable.create("s", "/live.md", "truth").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir = temp("fresh-projection");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deleted.md"), "stale").unwrap();

        let guard = mounter
            .mount("s", &dir, MountAccess::ReadOnly)
            .await
            .mounted();
        assert_eq!(
            std::fs::read_to_string(dir.join("live.md")).unwrap(),
            "truth"
        );
        assert!(
            !dir.join("deleted.md").exists(),
            "a memory deleted from store truth cannot resurrect on remount"
        );
        guard.teardown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn projection_cleanup_removes_directory_and_never_follows_symlink() {
        let root = temp("clear-projection");
        let outside = temp("clear-projection-outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        let projection = root.join("memory");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &projection).unwrap();

        #[cfg(unix)]
        {
            clear_projection(&projection).unwrap();
            assert!(!projection.exists());
            assert_eq!(
                std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
                "keep"
            );
        }

        std::fs::create_dir_all(&projection).unwrap();
        std::fs::write(projection.join("stale.txt"), "stale").unwrap();
        clear_projection(&projection).unwrap();
        assert!(!projection.exists());
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(outside).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fuse_constructor_refuses_a_second_bus_before_any_listener_or_mount() {
        let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
        let original = Arc::new(LocalInvalidator::default());
        let second = Arc::new(LocalInvalidator::default());
        let decorated = Arc::new(
            awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                backend,
                original.clone(),
            ),
        );
        let source = MemoryMountSource::new(decorated.clone());
        assert_eq!(original.subscriber_count(), 0);
        assert_eq!(second.subscriber_count(), 0);
        let result =
            MemoryStoreMounter::fuse_preferred(source, second.clone(), awaken_time::system_clock());
        assert!(matches!(
            result,
            Err(MemoryMounterConstructionError::InvalidationBusMismatch)
        ));
        assert_eq!(
            original.subscriber_count(),
            0,
            "no original-bus listener was started"
        );
        assert_eq!(
            second.subscriber_count(),
            0,
            "no second-bus listener was started"
        );
        assert_eq!(
            Arc::strong_count(&decorated),
            1,
            "rejected construction retains no projection or mount task"
        );
    }
}
