//! Data-plane adapter for one already-authorized platform MemoryStore binding.
//!
//! Resource resolution owns the binding identity and maximum access. This
//! adapter only translates the common Memory tool/extraction operations onto
//! that exact store; it owns no catalog, lifecycle, or extraction-outbox truth.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_memory::{
    MemoryExtractionMutation, MemoryMutationReceipt, MemoryStoreHandle, sanitize_stem,
};

pub(crate) struct PlatformMemoryHandle {
    fs: Arc<dyn awaken_resource_contract::MemoryRepository>,
    store_id: String,
    writable: bool,
}

impl PlatformMemoryHandle {
    pub(crate) fn new(
        fs: Arc<dyn awaken_resource_contract::MemoryRepository>,
        store_id: String,
        writable: bool,
    ) -> Self {
        Self {
            fs,
            store_id,
            writable,
        }
    }

    pub(crate) async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
        self.fs
            .list(&self.store_id, prefix)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn read(
        &self,
        path: &str,
    ) -> Result<Option<awaken_resource_contract::Memory>, String> {
        self.fs
            .get_by_path(&self.store_id, path)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn write_exact(
        &self,
        path: &str,
        content: &str,
        expected_sha256: Option<&str>,
    ) -> Result<awaken_resource_contract::Memory, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        match expected_sha256 {
            None => self
                .fs
                .create(&self.store_id, path, content)
                .await
                .map_err(|error| error.to_string()),
            Some(expected) => {
                let current = self
                    .fs
                    .get_by_path(&self.store_id, path)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("memory not found: {path}"))?;
                self.fs
                    .update(&self.store_id, &current.id, content, expected)
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }

    pub(crate) async fn delete_exact(
        &self,
        path: &str,
        expected_id: &str,
        expected_sha256: &str,
    ) -> Result<bool, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        self.fs
            .delete_if_match(&self.store_id, path, expected_id, expected_sha256)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn plan_mutations(
        &self,
        writes: BTreeMap<String, String>,
    ) -> Result<Vec<MemoryExtractionMutation>, String> {
        let mut mutations = Vec::with_capacity(writes.len());
        for (path, content) in writes {
            let current = self
                .fs
                .get_by_path(&self.store_id, &path)
                .await
                .map_err(|error| error.to_string())?;
            mutations.push(MemoryExtractionMutation {
                path,
                target_sha256: awaken_resource_contract::memory_sha256_hex(&content),
                content,
                observed_sha256: current.map(|memory| memory.content_sha256),
            });
        }
        Ok(mutations)
    }

    pub(super) async fn apply_mutation(
        &self,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        if !self.writable {
            return Err("memory store binding is read-only".into());
        }
        let current = self
            .fs
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
        match (current, mutation.observed_sha256.as_deref()) {
            (None, None) => {
                self.fs
                    .create(&self.store_id, &mutation.path, &mutation.content)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            (Some(current), Some(expected)) if current.content_sha256 == expected => {
                self.fs
                    .update(&self.store_id, &current.id, &mutation.content, expected)
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
            .fs
            .get_by_path(&self.store_id, &path)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(current) => self
                .fs
                .update(
                    &self.store_id,
                    &current.id,
                    content,
                    &current.content_sha256,
                )
                .await
                .map_err(|error| error.to_string())?,
            None => self
                .fs
                .create(&self.store_id, &path, content)
                .await
                .map_err(|error| error.to_string())?,
        };
        Ok(path)
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        let mut entries = Vec::new();
        for memory in self
            .fs
            .snapshot_heads(&self.store_id)
            .await
            .map_err(|error| error.to_string())?
        {
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
        entries.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(entries)
    }
}
