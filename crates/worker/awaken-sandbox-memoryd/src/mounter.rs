//! The provider-facing memory-store realization (ADR-0053, item 1).
//!
//! [`MemoryStoreMounter`] implements the neutral [`MemoryMounter`] port a sandbox
//! provider calls to realize a `MountSource::MemoryStore`. It exposes the store as a
//! **live FUSE mount** where the kernel supports it ([`fuse_available`]), and
//! otherwise falls back to a **copy** that is harvested back to the store on teardown
//! (ADR-0053 D6). Both paths write through the same durable [`MemoryRepository`], wrapped in
//! an [`InvalidatingMemoryRepository`] over one shared bus so a write in one sandbox drops the
//! path from every other sandbox's FUSE cache (the D5 coherence model, applied
//! per-sandbox rather than via a single shared mount — no bind/namespace splice
//! needed, so it works on the unprivileged Workdir tier).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_memory_store::MemoryRepository;
use awaken_provisioning_contract::{
    MemoryMount, MemoryMounter, MountAccess, Realization, SandboxError,
};

use crate::copy;
use crate::invalidate::{InvalidatingMemoryRepository, Invalidator, LocalInvalidator};

/// Realizes memory stores over one durable [`MemoryRepository`], FUSE-first with a copy
/// fallback. Construct once per host and inject into the sandbox providers.
pub struct MemoryStoreMounter {
    /// The durable store wrapped so every write publishes an invalidation.
    fs: Arc<dyn MemoryRepository>,
    /// The shared invalidation bus every FUSE mount subscribes to.
    bus: Arc<LocalInvalidator>,
    /// When false, never mount FUSE — always copy. Set for isolation tiers that
    /// cannot splice a host FUSE mount into their namespace yet (bwrap/container;
    /// live-FUSE-in-namespace is ADR-0053 item 2, deferred). The Workdir tier runs in
    /// the host mount namespace, so it FUSE-mounts directly at the sandbox path.
    prefer_fuse: bool,
}

impl MemoryStoreMounter {
    /// Wrap `durable` (the resources-plane store) with the invalidation bus, FUSE
    /// where available. For the Workdir tier (host mount namespace).
    #[must_use]
    pub fn new(durable: Arc<dyn MemoryRepository>) -> Self {
        Self::with_fuse(durable, true)
    }

    /// A copy-only mounter (never FUSE) for a tier that cannot expose a host FUSE
    /// mount inside its isolation (bwrap/container). The store is materialized to
    /// plain files that bind into the namespace, and harvested back on teardown.
    #[must_use]
    pub fn copy_only(durable: Arc<dyn MemoryRepository>) -> Self {
        Self::with_fuse(durable, false)
    }

    fn with_fuse(durable: Arc<dyn MemoryRepository>, prefer_fuse: bool) -> Self {
        let bus = Arc::new(LocalInvalidator::default());
        let invalidator: Arc<dyn Invalidator> = bus.clone();
        let fs: Arc<dyn MemoryRepository> =
            Arc::new(InvalidatingMemoryRepository::new(durable, invalidator));
        Self {
            fs,
            bus,
            prefer_fuse,
        }
    }
}

fn sandbox_err(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::new(e.to_string())
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

#[cfg(all(feature = "fuse", not(target_os = "linux")))]
fn is_mountpoint(_path: &Path) -> bool {
    false
}

#[cfg(feature = "fuse")]
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
    #[cfg(feature = "fuse")]
    if is_mountpoint(host_path) {
        detach_fuse(host_path);
    }
    let Ok(metadata) = std::fs::symlink_metadata(host_path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return std::fs::remove_file(host_path).map_err(sandbox_err);
    }
    if std::fs::remove_dir_all(host_path).is_ok() && matches!(host_path.try_exists(), Ok(false)) {
        return Ok(());
    }

    #[cfg(feature = "fuse")]
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
    Err(sandbox_err(format!(
        "clear prior Memory projection `{}`: {}",
        host_path.display(),
        last_error.map_or_else(
            || "mountpoint remained after removal".into(),
            |error| error.to_string()
        )
    )))
}

#[async_trait::async_trait]
impl MemoryMounter for MemoryStoreMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError> {
        // Every realization is a fresh projection. In particular, a deleted
        // memory from the durable store must not reappear from stale copy bytes
        // left by an evicted Session context. Never follow a replaced symlink.
        clear_projection(host_path)?;
        std::fs::create_dir_all(host_path).map_err(|error| {
            sandbox_err(format!(
                "create Memory projection `{}`: {error}",
                host_path.display()
            ))
        })?;

        #[cfg(feature = "fuse")]
        if self.prefer_fuse && copy::fuse_available() {
            let handle = crate::fuse::spawn_mount_with_invalidations(
                self.fs.clone(),
                store_id.to_string(),
                host_path.to_path_buf(),
                self.bus.subscribe(),
            )
            .map_err(|error| {
                sandbox_err(format!(
                    "mount MemoryStore `{store_id}` at `{}`: {error}",
                    host_path.display()
                ))
            })?;
            return Ok(Box::new(FuseMount { handle }));
        }

        // No FUSE (macOS / CI / unprivileged container): copy the store out now and
        // harvest a writable mount back on teardown.
        let snapshot = copy::materialize(&*self.fs, store_id, host_path)
            .await
            .map_err(sandbox_err)?;
        Ok(Box::new(CopyMount {
            fs: self.fs.clone(),
            store_id: store_id.to_string(),
            host_path: host_path.to_path_buf(),
            writable: access == MountAccess::ReadWrite,
            snapshot,
        }))
    }
}

/// A live FUSE mount; teardown unmounts (draining open fds).
#[cfg(feature = "fuse")]
struct FuseMount {
    handle: crate::fuse::MemoryMountHandle,
}

#[cfg(feature = "fuse")]
#[async_trait::async_trait]
impl MemoryMount for FuseMount {
    fn realization(&self) -> Realization {
        Realization::Fuse
    }

    async fn teardown(self: Box<Self>) {
        // `unmount` blocks briefly (drains fds, joins the session); keep it off the
        // async worker.
        let handle = self.handle;
        let _ = tokio::task::spawn_blocking(move || handle.unmount()).await;
    }
}

/// A materialized copy; teardown harvests a writable copy back to the store.
struct CopyMount {
    fs: Arc<dyn MemoryRepository>,
    store_id: String,
    host_path: PathBuf,
    writable: bool,
    snapshot: copy::CopySnapshot,
}

#[async_trait::async_trait]
impl MemoryMount for CopyMount {
    fn realization(&self) -> Realization {
        Realization::Copy
    }

    async fn teardown(self: Box<Self>) {
        if self.writable {
            let mut snapshot = self.snapshot;
            match copy::harvest(&*self.fs, &self.store_id, &self.host_path, &mut snapshot).await {
                Ok(report) if !report.conflicts.is_empty() => tracing::warn!(
                    store = %self.store_id,
                    conflicts = ?report.conflicts,
                    "memory copy harvest preserved concurrent durable heads"
                ),
                Err(error) => {
                    tracing::warn!(store = %self.store_id, %error, "memory copy harvest failed");
                }
                Ok(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::VolatileMemoryRepository;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let durable = Arc::new(VolatileMemoryRepository::new());
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::new(durable.clone());
        let dir = temp("copy");

        let snapshot = copy::materialize(&*mounter.fs, "s", &dir).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");

        // Agent edits the file; the writable copy guard harvests it back.
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            fs: mounter.fs.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: true,
            snapshot,
        });
        assert_eq!(guard.realization(), Realization::Copy);
        guard.teardown().await;

        assert_eq!(
            durable
                .get_by_path("s", "/note.md")
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
    async fn a_read_only_copy_mount_does_not_harvest() {
        let durable = Arc::new(VolatileMemoryRepository::new());
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::new(durable.clone());
        let dir = temp("ro");
        let snapshot = copy::materialize(&*mounter.fs, "s", &dir).await.unwrap();

        // A read-only edit on disk must NOT propagate back.
        std::fs::write(dir.join("note.md"), "tampered").unwrap();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            fs: mounter.fs.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: false,
            snapshot,
        });
        guard.teardown().await;

        assert_eq!(
            durable
                .get_by_path("s", "/note.md")
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
        let durable = Arc::new(VolatileMemoryRepository::new());
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
            .unwrap();
        let gb = mounter
            .mount("beta", &dir_b, MountAccess::ReadOnly)
            .await
            .unwrap();

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
            durable
                .get_by_path("beta", "/secret.md")
                .await
                .unwrap()
                .is_none(),
            "beta cannot read alpha's memory"
        );
        assert!(
            durable
                .get_by_path("alpha", "/note.md")
                .await
                .unwrap()
                .is_none(),
            "alpha cannot read beta's memory"
        );

        ga.teardown().await;
        gb.teardown().await;
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
        let durable = Arc::new(VolatileMemoryRepository::new());
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable.clone());
        let dir = temp("copyonly");

        let guard = mounter
            .mount("s", &dir, MountAccess::ReadWrite)
            .await
            .unwrap();
        assert_eq!(guard.realization(), Realization::Copy);
        // The store was materialized into the host dir (no FUSE).
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");

        // An edit is harvested back on teardown.
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        guard.teardown().await;
        assert_eq!(
            durable
                .get_by_path("s", "/note.md")
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

        let durable = Arc::new(VolatileMemoryRepository::new());
        durable.create("s", "/live.md", "truth").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(durable);
        let dir = temp("fresh-projection");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deleted.md"), "stale").unwrap();

        let guard = mounter
            .mount("s", &dir, MountAccess::ReadOnly)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("live.md")).unwrap(),
            "truth"
        );
        assert!(
            !dir.join("deleted.md").exists(),
            "a memory deleted from store truth cannot resurrect on remount"
        );
        guard.teardown().await;
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
}
