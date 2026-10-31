//! T40-C02: a remote client that offers only CopyOnly capabilities keeps a
//! working copy realization through the public API, and patching a raw
//! full-permission backend in to obtain a live projection is refused.
//!
//! The compile-time half (a CopyOnly-incomplete client and a raw backend
//! cannot construct a mounter or a `MemoryMountSource`) lives in the
//! `compile_fail` doctests on `MemoryStoreMounter::copy_only` and
//! `MemoryStoreMounter::fuse_preferred`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_memory_store::VolatileMemoryRepository;
use awaken_provisioning_contract::{MemoryMounter, MountAccess, Realization};
use awaken_resource_contract::{
    LocalInvalidator, MemErr, Memory, MemoryActor, MemoryCopyConflict,
    MemoryCopyConflictPreservation, MemoryCopyWriteback, MemoryEntry, MemoryFilesystem,
    MemoryGeneration, MemoryInvalidationIdentity, MemoryInvalidator, MemoryMountSource,
    MemoryQueries, MemorySnapshotSource, MemoryWriter,
};
use awaken_sandbox_memoryd::{MemoryMounterConstructionError, MemoryStoreMounter};
use support::MemoryMountOutcomeTestExt as _;

/// The remote server's state is the volatile repository; the client exposes
/// exactly the CopyOnly roles and counts every CAS write-back it forwards.
struct CopyOnlyRemote {
    server: Arc<VolatileMemoryRepository>,
    writebacks: AtomicUsize,
}

#[async_trait::async_trait]
impl MemoryQueries for CopyOnlyRemote {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryQueries::list(self.server.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryQueries::get_by_path(self.server.as_ref(), store, path).await
    }
}

#[async_trait::async_trait]
impl MemorySnapshotSource for CopyOnlyRemote {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.server.snapshot_heads(store).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyWriteback for CopyOnlyRemote {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.writebacks.fetch_add(1, Ordering::SeqCst);
        MemoryCopyWriteback::create_as(self.server.as_ref(), store, path, content, actor).await
    }
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.writebacks.fetch_add(1, Ordering::SeqCst);
        self.server
            .update_materialized(store, path, base, content)
            .await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        self.writebacks.fetch_add(1, Ordering::SeqCst);
        MemoryCopyWriteback::delete_if_match_as(self.server.as_ref(), store, path, base, actor)
            .await
    }
}

#[async_trait::async_trait]
impl MemoryCopyConflictPreservation for CopyOnlyRemote {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        self.server.preserve_copy_conflict(store, candidate).await
    }
}

fn remote(server: &Arc<VolatileMemoryRepository>) -> Arc<CopyOnlyRemote> {
    Arc::new(CopyOnlyRemote {
        server: server.clone(),
        writebacks: AtomicUsize::new(0),
    })
}

/// C1 the CopyOnly remote is the only Memory capability; C2 the public
/// `mount()` realizes it ReadWrite; C3 the agent edits the copy. E1 the
/// realization is Copy, materialized from the remote snapshot; E2 teardown
/// commits exactly one CAS write-back through the remote client and the
/// server head advances to the edit.
#[tokio::test]
async fn a_copy_only_remote_mounts_a_copy_and_writes_back_through_its_cas() {
    let server = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    server.create("s", "/note.md", "v1").await.unwrap();
    let client = remote(&server);
    let mounter = MemoryStoreMounter::copy_only(client.clone());
    let root = tempfile::tempdir().unwrap();
    let projection = root.path().join("projection");

    let mount = mounter
        .mount("s", &projection, MountAccess::ReadWrite)
        .await
        .mounted();
    assert_eq!(mount.realization(), Realization::Copy, "E1");
    assert_eq!(
        std::fs::read_to_string(projection.join("note.md")).unwrap(),
        "v1",
        "E1"
    );
    std::fs::write(projection.join("note.md"), "v2").unwrap();
    mount.teardown().await.unwrap();

    assert_eq!(client.writebacks.load(Ordering::SeqCst), 1, "E2");
    assert_eq!(
        MemoryQueries::get_by_path(server.as_ref(), "s", "/note.md")
            .await
            .unwrap()
            .and_then(|memory| memory.content),
        Some("v2".to_string()),
        "E2"
    );
}

/// Negative-control client: the CopyOnly remote "patched" with a raw
/// full-permission backend for the filesystem role and a private bus that it
/// claims as its invalidation identity.
struct PatchedRemote {
    copy: Arc<CopyOnlyRemote>,
    raw: Arc<VolatileMemoryRepository>,
    private_bus: Arc<LocalInvalidator>,
}

#[async_trait::async_trait]
impl MemoryQueries for PatchedRemote {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryQueries::list(self.copy.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryQueries::get_by_path(self.copy.as_ref(), store, path).await
    }
}

#[async_trait::async_trait]
impl MemorySnapshotSource for PatchedRemote {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.copy.snapshot_heads(store).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyWriteback for PatchedRemote {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryCopyWriteback::create_as(self.copy.as_ref(), store, path, content, actor).await
    }
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.copy
            .update_materialized(store, path, base, content)
            .await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        MemoryCopyWriteback::delete_if_match_as(self.copy.as_ref(), store, path, base, actor).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyConflictPreservation for PatchedRemote {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        self.copy.preserve_copy_conflict(store, candidate).await
    }
}

#[async_trait::async_trait]
impl MemoryFilesystem for PatchedRemote {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryFilesystem::list(self.raw.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryFilesystem::get_by_path(self.raw.as_ref(), store, path).await
    }
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryFilesystem::create_as(self.raw.as_ref(), store, path, content, actor).await
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryFilesystem::update_head_as(
            self.raw.as_ref(),
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await
    }
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        MemoryFilesystem::rename(self.raw.as_ref(), store, from, to).await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryFilesystem::delete_by_path_as(self.raw.as_ref(), store, path, actor).await
    }
}

impl MemoryInvalidationIdentity for PatchedRemote {
    fn invalidator_identity(&self) -> Arc<dyn MemoryInvalidator> {
        self.private_bus.clone()
    }
}

/// N1 the patched client compiles into a `MemoryMountSource`, but the root's
/// live projection admits only the root bus, so construction is refused
/// before any listener starts on either bus. N2 the refused patch wrote
/// nothing to the raw backend.
#[test]
fn a_raw_backend_patched_remote_cannot_join_the_root_live_projection() {
    let server = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let raw = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let private_bus = Arc::new(LocalInvalidator::new(8));
    let root_bus = Arc::new(LocalInvalidator::new(8));
    let patched = Arc::new(PatchedRemote {
        copy: remote(&server),
        raw: raw.clone(),
        private_bus: private_bus.clone(),
    });

    let refused = MemoryStoreMounter::fuse_preferred(
        MemoryMountSource::new(patched),
        root_bus.clone(),
        awaken_time::system_clock(),
    );
    assert!(
        matches!(
            refused,
            Err(MemoryMounterConstructionError::InvalidationBusMismatch)
        ),
        "N1 the patched raw backend is refused"
    );
    assert_eq!(root_bus.subscriber_count(), 0, "N1");
    assert_eq!(private_bus.subscriber_count(), 0, "N1");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(raw.snapshot_heads("s"))
            .unwrap()
            .is_empty(),
        "N2"
    );
}
