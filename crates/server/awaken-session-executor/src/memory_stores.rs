//! Memory content selected from one controlled backend binding; Resources owns storage.
#[cfg(test)]
use awaken_resource_contract::MemoryExecutionContent;

#[cfg(test)]
pub(crate) fn open(
    store_dir: Option<&std::path::Path>,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> MemoryExecutionContent {
    let backend = std::sync::Arc::new(test_support::OpenOnFirstUse {
        path: store_dir.map(|dir| {
            dir.join("memory_fs.db")
                .to_str()
                .expect("memory-fs db path is valid UTF-8")
                .to_owned()
        }),
        clock,
        repository: tokio::sync::OnceCell::new(),
    });
    MemoryExecutionContent::new(backend)
}

#[cfg(test)]
mod test_support {
    use std::sync::Arc;

    use awaken_memory_store::SqliteMemoryRepository;
    use awaken_resource_contract::{
        MemErr, Memory, MemoryActor, MemoryCopyWriteback, MemoryEntry, MemoryGeneration,
        MemoryQueries, MemorySnapshotSource, MemoryWriter,
    };

    /// The SQLite Memory repository of [`super::open`], opened on
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
    #[cfg(test)]
    impl MemoryQueries for OpenOnFirstUse {
        async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
            MemoryQueries::list(self.repository().await, store, prefix).await
        }
        async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
            MemoryQueries::get_by_path(self.repository().await, store, path).await
        }
    }
    #[async_trait::async_trait]
    #[cfg(test)]
    impl MemorySnapshotSource for OpenOnFirstUse {
        async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
            MemorySnapshotSource::snapshot_heads(self.repository().await, store).await
        }
    }
    #[async_trait::async_trait]
    #[cfg(test)]
    impl MemoryWriter for OpenOnFirstUse {
        async fn create_as(
            &self,
            store: &str,
            path: &str,
            content: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<Memory, MemErr> {
            MemoryWriter::create_as(self.repository().await, store, path, content, actor).await
        }
        async fn update_head_as(
            &self,
            store: &str,
            base: &MemoryGeneration,
            content: &str,
            target_path: Option<&str>,
            actor: Option<&MemoryActor>,
        ) -> Result<Memory, MemErr> {
            MemoryWriter::update_head_as(
                self.repository().await,
                store,
                base,
                content,
                target_path,
                actor,
            )
            .await
        }
        async fn delete_by_path_as(
            &self,
            store: &str,
            path: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<(), MemErr> {
            MemoryWriter::delete_by_path_as(self.repository().await, store, path, actor).await
        }
        async fn delete_if_match_as(
            &self,
            store: &str,
            path: &str,
            base: &MemoryGeneration,
            actor: Option<&MemoryActor>,
        ) -> Result<bool, MemErr> {
            MemoryWriter::delete_if_match_as(self.repository().await, store, path, base, actor)
                .await
        }
    }
    #[async_trait::async_trait]
    #[cfg(test)]
    impl MemoryCopyWriteback for OpenOnFirstUse {
        async fn create_as(
            &self,
            store: &str,
            path: &str,
            content: &str,
            actor: Option<&MemoryActor>,
        ) -> Result<Memory, MemErr> {
            MemoryCopyWriteback::create_as(self.repository().await, store, path, content, actor)
                .await
        }
        async fn update_materialized(
            &self,
            store: &str,
            path: &str,
            base: &MemoryGeneration,
            content: &str,
        ) -> Result<Memory, MemErr> {
            MemoryCopyWriteback::update_materialized(
                self.repository().await,
                store,
                path,
                base,
                content,
            )
            .await
        }
        async fn delete_if_match_as(
            &self,
            store: &str,
            path: &str,
            base: &MemoryGeneration,
            actor: Option<&MemoryActor>,
        ) -> Result<bool, MemErr> {
            MemoryCopyWriteback::delete_if_match_as(
                self.repository().await,
                store,
                path,
                base,
                actor,
            )
            .await
        }
    }
}
