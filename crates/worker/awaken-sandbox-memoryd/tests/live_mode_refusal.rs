//! T40-C03 capability refusal on hosts without kernel FUSE.
//!
//! This crate's live projection is `cfg(all(feature = "fuse", target_os =
//! "linux"))`, and its only invalidation transport is the in-process
//! `LocalInvalidator` broadcast. On any other host a mounter whose bus IS the
//! source's own invalidation identity (so `fuse_preferred` accepts it):
//!
//! | Rule | Effect |
//! |---|---|
//! | L1 | the mount realizes Copy and subscribes no listener on the bus |
//! | L2 | a later durable write plus its invalidation never reaches the mounted bytes |
//!
//! This is refusal evidence ("not applicable"), not consistency evidence: the
//! observed subscriber count is zero, so the NC shape (two subscribers of one
//! in-process broadcast) does not even arise.
#![cfg(not(target_os = "linux"))]

mod support;

use std::sync::Arc;

use awaken_memory_store::VolatileMemoryRepository;
use awaken_provisioning_contract::{MemoryMounter, MountAccess, Realization};
use awaken_resource_contract::{
    LocalInvalidator, MemErr, Memory, MemoryActor, MemoryCopyConflict,
    MemoryCopyConflictPreservation, MemoryCopyWriteback, MemoryEntry, MemoryFilesystem,
    MemoryGeneration, MemoryInvalidationIdentity, MemoryInvalidator, MemoryMountSource,
    MemoryQueries, MemorySnapshotSource, MemoryWriter,
};
use awaken_sandbox_memoryd::MemoryStoreMounter;
use support::MemoryMountOutcomeTestExt as _;

/// Every Memory role over one repository, claiming `bus` as its identity.
struct BusBound {
    repository: Arc<VolatileMemoryRepository>,
    bus: Arc<LocalInvalidator>,
}

#[async_trait::async_trait]
impl MemoryQueries for BusBound {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryQueries::list(self.repository.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryQueries::get_by_path(self.repository.as_ref(), store, path).await
    }
}

#[async_trait::async_trait]
impl MemorySnapshotSource for BusBound {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.repository.snapshot_heads(store).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyWriteback for BusBound {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryCopyWriteback::create_as(self.repository.as_ref(), store, path, content, actor).await
    }
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.repository
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
        MemoryCopyWriteback::delete_if_match_as(self.repository.as_ref(), store, path, base, actor)
            .await
    }
}

#[async_trait::async_trait]
impl MemoryCopyConflictPreservation for BusBound {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        self.repository
            .preserve_copy_conflict(store, candidate)
            .await
    }
}

#[async_trait::async_trait]
impl MemoryFilesystem for BusBound {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        MemoryFilesystem::list(self.repository.as_ref(), store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        MemoryFilesystem::get_by_path(self.repository.as_ref(), store, path).await
    }
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryFilesystem::create_as(self.repository.as_ref(), store, path, content, actor).await
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
            self.repository.as_ref(),
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await
    }
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        MemoryFilesystem::rename(self.repository.as_ref(), store, from, to).await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryFilesystem::delete_by_path_as(self.repository.as_ref(), store, path, actor).await
    }
}

impl MemoryInvalidationIdentity for BusBound {
    fn invalidator_identity(&self) -> Arc<dyn MemoryInvalidator> {
        self.bus.clone()
    }
}

#[tokio::test]
async fn t40_c03_without_kernel_fuse_a_matching_bus_realizes_copy_with_no_live_listener() {
    let repository = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    repository.create("s", "/note.md", "v1").await.unwrap();
    let bus = Arc::new(LocalInvalidator::new(8));
    let mounter = MemoryStoreMounter::fuse_preferred(
        MemoryMountSource::new(Arc::new(BusBound {
            repository: repository.clone(),
            bus: bus.clone(),
        })),
        bus.clone(),
        awaken_time::system_clock(),
    )
    .expect("the source's own bus is accepted");
    let root = tempfile::tempdir().unwrap();
    let projection = root.path().join("projection");

    let mount = mounter
        .mount("s", &projection, MountAccess::ReadOnly)
        .await
        .mounted();
    assert_eq!(
        mount.realization(),
        Realization::Copy,
        "L1 no FUSE realization"
    );
    assert_eq!(
        bus.subscriber_count(),
        0,
        "L1 no live invalidation listener"
    );

    repository
        .create("s", "/late.md", "after mount")
        .await
        .unwrap();
    bus.publish("s", "/late.md");
    bus.publish("s", "/note.md");
    assert!(
        !projection.join("late.md").exists(),
        "L2 a later write is not projected live"
    );
    assert_eq!(
        std::fs::read_to_string(projection.join("note.md")).unwrap(),
        "v1",
        "L2 the copy keeps its mount-time bytes"
    );
    assert_eq!(bus.subscriber_count(), 0, "L2");
    mount.teardown().await.unwrap();
}
