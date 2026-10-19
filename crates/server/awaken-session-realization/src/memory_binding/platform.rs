// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: Runtime Host adapts the frozen Session Memory binding to the canonical
// @formal reason: MemoryWriter complete-generation API without owning a second CAS rule. Dynamic path:
// @formal reason: create has no predecessor, while write, delete and extraction replay pass the exact
// @formal reason: immutable-id/version/content token into the repository; path-sensitive operations use
// @formal reason: update_materialized so a renamed or replaced materialization conflicts. Repository locks
// @formal reason: and transactions remain the only mutation authorities, and adjacent runtime tests
// @formal reason: provide executable wiring evidence rather than a direct backend or durability proof.
//! Data-plane adapter for one already-authorized platform MemoryStore binding.
//!
//! Resource resolution owns the binding identity and maximum access. This
//! adapter only translates the common Memory tool/extraction operations onto
//! that exact store; it owns no catalog, lifecycle, or extraction-outbox truth.

use std::collections::BTreeMap;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_memory::{
    MemoryExtractionMutation, MemoryHeadToken, MemoryMutationReceipt, MemoryStoreHandle,
    sanitize_stem,
};

pub struct PlatformMemoryHandle {
    content: awaken_resource_contract::MemoryExecutionContent,
    store_id: String,
    writable: bool,
}

impl PlatformMemoryHandle {
    pub async fn entries_from_snapshot(
        source: &dyn awaken_resource_contract::MemorySnapshotSource,
        store_id: &str,
    ) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        let mut memories = source
            .snapshot_heads(store_id)
            .await
            .map_err(|error| error.to_string())?;
        // Keep source precision: platform SystemTime can round distinct heads
        // to the same instant, and its representable range is narrower.
        memories.sort_by(|left, right| {
            right
                .updated_unix_nanos
                .cmp(&left.updated_unix_nanos)
                .then_with(|| left.path.cmp(&right.path))
        });
        let mut entries = Vec::new();
        for memory in memories {
            let Some(content) = memory.content.filter(|content| !content.trim().is_empty()) else {
                continue;
            };
            let nanos = u64::try_from(memory.updated_unix_nanos).unwrap_or(u64::MAX);
            entries.push(awaken_ext_memory::Entry {
                path: std::path::PathBuf::from(memory.path),
                content: content.trim().to_string(),
                modified: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos),
            });
        }
        Ok(entries)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new<M>(backend: Arc<M>, store_id: String, writable: bool) -> Self
    where
        M: awaken_resource_contract::MemoryQueries
            + awaken_resource_contract::MemorySnapshotSource
            + awaken_resource_contract::MemoryWriter
            + awaken_resource_contract::MemoryCopyWriteback
            + 'static,
    {
        Self::from_roles(
            awaken_resource_contract::MemoryExecutionContent::new(backend.clone()),
            store_id,
            writable,
        )
    }

    pub fn from_roles(
        content: awaken_resource_contract::MemoryExecutionContent,
        store_id: String,
        writable: bool,
    ) -> Self {
        Self {
            content,
            store_id,
            writable,
        }
    }

    pub async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
        self.content
            .queries()
            .list(&self.store_id, prefix)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn read(
        &self,
        path: &str,
    ) -> Result<Option<awaken_resource_contract::Memory>, String> {
        self.content
            .queries()
            .get_by_path(&self.store_id, path)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn write_exact(
        &self,
        path: &str,
        content: &str,
        expected_generation: Option<&awaken_resource_contract::MemoryGeneration>,
    ) -> Result<awaken_resource_contract::Memory, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        match expected_generation {
            None => self
                .content
                .writer()
                .create(&self.store_id, path, content)
                .await
                .map_err(|error| error.to_string()),
            Some(expected) => self
                .content
                .copy_writeback()
                .update_materialized(&self.store_id, path, expected, content)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    pub async fn delete_exact(
        &self,
        path: &str,
        expected_generation: &awaken_resource_contract::MemoryGeneration,
    ) -> Result<bool, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        self.content
            .writer()
            .delete_if_match(&self.store_id, path, expected_generation)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn plan_mutations(
        &self,
        writes: BTreeMap<String, String>,
    ) -> Result<Vec<MemoryExtractionMutation>, String> {
        let mut mutations = Vec::with_capacity(writes.len());
        for (path, content) in writes {
            let current = self
                .content
                .queries()
                .get_by_path(&self.store_id, &path)
                .await
                .map_err(|error| error.to_string())?;
            let observed_generation = current
                .map(|memory| {
                    serde_json::to_value(memory.generation())
                        .map(MemoryHeadToken::new)
                        .map_err(|error| format!("cannot encode the observed memory head: {error}"))
                })
                .transpose()?;
            mutations.push(MemoryExtractionMutation {
                path,
                target_sha256: awaken_resource_contract::memory_sha256_hex(&content),
                content,
                observed_generation,
            });
        }
        Ok(mutations)
    }

    pub async fn apply_mutation(
        &self,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        let current = self
            .content
            .queries()
            .get_by_path(&self.store_id, &mutation.path)
            .await
            .map_err(|error| error.to_string())?;
        if current
            .as_ref()
            .is_some_and(|memory| memory.content_sha256 == mutation.target_sha256)
        {
            return Ok(MemoryMutationReceipt {
                path: mutation.path.clone(),
                target_sha256: mutation.target_sha256.clone(),
                already_applied: true,
            });
        }
        // The token is this store's own encoding; one it cannot decode is
        // refused rather than read as a stale head.
        let expected = mutation
            .observed_generation
            .as_ref()
            .map(|token| {
                serde_json::from_value::<awaken_resource_contract::MemoryGeneration>(
                    token.encoded().clone(),
                )
                .map_err(|error| format!("unreadable observed memory head: {error}"))
            })
            .transpose()?;
        match (current, expected.as_ref()) {
            (None, None) => {
                self.content
                    .writer()
                    .create(&self.store_id, &mutation.path, &mutation.content)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            (Some(current), Some(expected)) if current.generation() == *expected => {
                self.content
                    .copy_writeback()
                    .update_materialized(
                        &self.store_id,
                        &mutation.path,
                        expected,
                        &mutation.content,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            _ => {
                return Err(format!(
                    "memory `{}` changed after extraction planning",
                    mutation.path
                ));
            }
        }
        Ok(MemoryMutationReceipt {
            path: mutation.path.clone(),
            target_sha256: mutation.target_sha256.clone(),
            already_applied: false,
        })
    }
}

#[async_trait]
impl MemoryStoreHandle for PlatformMemoryHandle {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        let path = format!("/{}.md", sanitize_stem(name));
        match self
            .content
            .queries()
            .get_by_path(&self.store_id, &path)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(current) => self
                .content
                .writer()
                .update(&self.store_id, &current.generation(), content)
                .await
                .map_err(|error| error.to_string())?,
            None => self
                .content
                .writer()
                .create(&self.store_id, &path, content)
                .await
                .map_err(|error| error.to_string())?,
        };
        Ok(path)
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        Self::entries_from_snapshot(self.content.snapshots().as_ref(), &self.store_id).await
    }
}
