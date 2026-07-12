//! The provider-facing memory-store realization (ADR-0053, item 1).
//!
//! [`MemoryStoreMounter`] implements the neutral [`MemoryMounter`] port a sandbox
//! provider calls to realize a `MountSource::MemoryStore`. It exposes the store as a
//! **live FUSE mount** where the kernel supports it ([`fuse_available`]), and
//! otherwise falls back to a **copy** that is harvested back to the store on teardown
//! (ADR-0053 D6). Both paths write through the same durable [`MemoryFs`], wrapped in
//! an [`InvalidatingMemoryFs`] over one shared bus so a write in one sandbox drops the
//! path from every other sandbox's FUSE cache (the D5 coherence model, applied
//! per-sandbox rather than via a single shared mount — no bind/namespace splice
//! needed, so it works on the unprivileged Workdir tier).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_memory_store::MemoryFs;
use awaken_provisioning_contract::{
    MemoryMount, MemoryMounter, MountAccess, Realization, SandboxError,
};

use crate::copy;
use crate::invalidate::{InvalidatingMemoryFs, Invalidator, LocalInvalidator};

/// Realizes memory stores over one durable [`MemoryFs`], FUSE-first with a copy
/// fallback. Construct once per host and inject into the sandbox providers.
pub struct MemoryStoreMounter {
    /// The durable store wrapped so every write publishes an invalidation.
    fs: Arc<dyn MemoryFs>,
    /// The shared invalidation bus every FUSE mount subscribes to.
    bus: Arc<LocalInvalidator>,
}

impl MemoryStoreMounter {
    /// Wrap `durable` (the resources-plane store) with the invalidation bus.
    #[must_use]
    pub fn new(durable: Arc<dyn MemoryFs>) -> Self {
        let bus = Arc::new(LocalInvalidator::default());
        let invalidator: Arc<dyn Invalidator> = bus.clone();
        let fs: Arc<dyn MemoryFs> = Arc::new(InvalidatingMemoryFs::new(durable, invalidator));
        Self { fs, bus }
    }
}

fn sandbox_err(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::new(e.to_string())
}

#[async_trait::async_trait]
impl MemoryMounter for MemoryStoreMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError> {
        std::fs::create_dir_all(host_path).map_err(sandbox_err)?;

        #[cfg(feature = "fuse")]
        if copy::fuse_available() {
            let handle = crate::fuse::spawn_mount_with_invalidations(
                self.fs.clone(),
                store_id.to_string(),
                host_path.to_path_buf(),
                self.bus.subscribe(),
            )
            .map_err(sandbox_err)?;
            return Ok(Box::new(FuseMount { handle }));
        }

        // No FUSE (macOS / CI / unprivileged container): copy the store out now and
        // harvest a writable mount back on teardown.
        copy::materialize(&*self.fs, store_id, host_path)
            .await
            .map_err(sandbox_err)?;
        Ok(Box::new(CopyMount {
            fs: self.fs.clone(),
            store_id: store_id.to_string(),
            host_path: host_path.to_path_buf(),
            writable: access == MountAccess::ReadWrite,
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
    fs: Arc<dyn MemoryFs>,
    store_id: String,
    host_path: PathBuf,
    writable: bool,
}

#[async_trait::async_trait]
impl MemoryMount for CopyMount {
    fn realization(&self) -> Realization {
        Realization::Copy
    }

    async fn teardown(self: Box<Self>) {
        if self.writable
            && let Err(e) = copy::harvest(&*self.fs, &self.store_id, &self.host_path).await
        {
            tracing::warn!(store = %self.store_id, error = %e, "memory copy harvest failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_memory_store::InMemoryFs;
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
        let durable = Arc::new(InMemoryFs::new());
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::new(durable.clone());
        let dir = temp("copy");

        copy::materialize(&*mounter.fs, "s", &dir).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");

        // Agent edits the file; the writable copy guard harvests it back.
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            fs: mounter.fs.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: true,
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
        let durable = Arc::new(InMemoryFs::new());
        durable.create("s", "/note.md", "v1").await.unwrap();
        let mounter = MemoryStoreMounter::new(durable.clone());
        let dir = temp("ro");
        copy::materialize(&*mounter.fs, "s", &dir).await.unwrap();

        // A read-only edit on disk must NOT propagate back.
        std::fs::write(dir.join("note.md"), "tampered").unwrap();
        let guard: Box<dyn MemoryMount> = Box::new(CopyMount {
            fs: mounter.fs.clone(),
            store_id: "s".into(),
            host_path: dir.clone(),
            writable: false,
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
}
