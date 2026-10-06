//! `MemoryStores` — the Resources context's Memory content backends (ADR-0038/0053).
//!
//! Owns the one path-addressed CAS [`MemoryRepository`] used by the Memory API, sandbox
//! mounts, recall, and extraction. Product startup injects the selected
//! repository; the local opener exists only for tests and scenario fixtures.
//!
//! Resource definition/configuration/lifecycle lives in the platform
//! `ResourceRegistry`, injected at the server process startup. The runtime host owns
//! only content backends; it does not own an authorization policy or a second identity
//! registry.

#[cfg(test)]
use std::path::Path;
use std::sync::Arc;

use awaken_resource_contract::MemoryRepository;

/// The Resources context's Memory content backend. See the module docs for why
/// identity/configuration is kept out.
pub(crate) struct MemoryStores {
    /// Durable, path-addressed memory files (ADR-0053): many memories, each at a path
    /// with a `content_sha256` + monotonic version, updated under compare-and-swap.
    fs: Arc<dyn MemoryRepository>,
}

impl MemoryStores {
    pub(crate) fn with_repository(fs: Arc<dyn MemoryRepository>) -> Self {
        Self { fs }
    }

    /// Opener for this crate's own unit tests under one storage-dir durability
    /// rule (`Some` → durable under the dir, `None` → ephemeral per-process).
    #[cfg(test)]
    pub(crate) fn open(store_dir: Option<&Path>, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        // ADR-0053 path-addressed memory files persist alongside, under the same
        // durability rule. Backed by the SQLite store so rename-replace and CAS are
        // **crash-atomic** (one transaction) — the plain-file backend is only no-loss
        // (a crash mid-rename can leave a transient duplicate source).
        //
        // Host construction is synchronous while opening the SQLite pool is not,
        // so this test-only opener defers the open to the first Memory call (until
        // test hosts are built from `HostDependencies`).
        let fs: Arc<dyn MemoryRepository> = Arc::new(test_support::OpenOnFirstUse {
            // Keep the original physical filename for an in-place upgrade; it is
            // storage layout, not the public/domain name of the repository port.
            // `None` → an ephemeral in-memory database (dies with the process).
            path: store_dir.map(|dir| {
                dir.join("memory_fs.db")
                    .to_str()
                    .expect("memory-fs db path is valid UTF-8")
                    .to_owned()
            }),
            clock,
            repository: tokio::sync::OnceCell::new(),
        });
        Self { fs }
    }

    #[cfg(test)]
    pub(crate) fn fs(&self) -> &Arc<dyn MemoryRepository> {
        &self.fs
    }

    pub(crate) fn fs_handle(&self) -> Arc<dyn MemoryRepository> {
        self.fs.clone()
    }
}

#[cfg(test)]
mod test_support {
    use std::sync::Arc;

    use awaken_memory_store::SqliteMemoryRepository;
    use awaken_resource_contract::{
        MemErr, Memory, MemoryActor, MemoryCopyConflict, MemoryEntry, MemoryGeneration,
        MemoryPurgeSummary, MemoryRepository, MemoryVersion,
    };

    /// The SQLite Memory repository of [`super::MemoryStores::open`], opened on
    /// its first call and then delegated to unchanged.
    pub(super) struct OpenOnFirstUse {
        pub(super) path: Option<String>,
        pub(super) clock: Arc<dyn awaken_time::WallClock>,
        pub(super) repository: tokio::sync::OnceCell<SqliteMemoryRepository>,
    }

    impl OpenOnFirstUse {
        async fn repository(&self) -> &SqliteMemoryRepository {
            let (path, clock) = (self.path.clone(), self.clock.clone());
            self.repository
                .get_or_init(|| async move {
                    match path {
                        Some(path) => SqliteMemoryRepository::open(&path, clock)
                            .await
                            .expect("open durable memory-fs sqlite store"),
                        None => SqliteMemoryRepository::open_in_memory(clock)
                            .await
                            .expect("open ephemeral memory-fs sqlite store"),
                    }
                })
                .await
        }
    }

    #[async_trait::async_trait]
    impl MemoryRepository for OpenOnFirstUse {
        async fn preserve_copy_conflict(
            &self,
            store: &str,
            candidate: &MemoryCopyConflict,
        ) -> Result<(), MemErr> {
            self.repository()
                .await
                .preserve_copy_conflict(store, candidate)
                .await
        }

        async fn list_copy_conflicts(
            &self,
            store: &str,
        ) -> Result<Vec<MemoryCopyConflict>, MemErr> {
            self.repository().await.list_copy_conflicts(store).await
        }

        async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
            self.repository().await.snapshot_heads(store).await
        }

        async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
            self.repository().await.list(store, prefix).await
        }

        async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
            self.repository().await.get_by_path(store, path).await
        }

        async fn create_as(
            &self,
            store: &str,
            path: &str,
            content: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<Memory, MemErr> {
            self.repository()
                .await
                .create_as(store, path, content, actor)
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
            self.repository()
                .await
                .update_head_as(store, base, content, target_path, actor)
                .await
        }

        async fn update_materialized(
            &self,
            store: &str,
            path: &str,
            base: &MemoryGeneration,
            content: &str,
        ) -> Result<Memory, MemErr> {
            self.repository()
                .await
                .update_materialized(store, path, base, content)
                .await
        }

        async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
            self.repository().await.rename(store, from, to).await
        }

        async fn delete_by_path_as(
            &self,
            store: &str,
            path: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<(), MemErr> {
            self.repository()
                .await
                .delete_by_path_as(store, path, actor)
                .await
        }

        async fn delete_if_match_as(
            &self,
            store: &str,
            path: &str,
            base: &MemoryGeneration,
            actor: Option<&MemoryActor>,
        ) -> Result<bool, MemErr> {
            self.repository()
                .await
                .delete_if_match_as(store, path, base, actor)
                .await
        }

        async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
            self.repository().await.list_versions(store).await
        }

        async fn redact_version_as(
            &self,
            store: &str,
            version_id: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<Option<MemoryVersion>, MemErr> {
            self.repository()
                .await
                .redact_version_as(store, version_id, actor)
                .await
        }

        async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
            self.repository().await.purge_store(store).await
        }
    }
}
