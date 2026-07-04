use std::path::PathBuf;
use std::sync::Arc;

use awaken_file_store::FileStore;

use crate::error::SandboxError;
use crate::mount::{Mount, MountAccess, MountSource};

/// A materialized bind mount descriptor for use with Docker `HostConfig.Binds`.
#[derive(Debug, Clone)]
pub struct DockerBind {
    /// Absolute path on the Docker host where the blob has been staged.
    pub host_path: PathBuf,
    /// Absolute path inside the container where the bind mount appears.
    pub container_path: PathBuf,
    /// Whether the bind is read-only.
    pub read_only: bool,
}

impl DockerBind {
    /// Returns the Docker bind specification string:
    /// `"<host_path>:<container_path>"` or `"<host_path>:<container_path>:ro"`.
    pub fn bind_spec(&self) -> String {
        let base = format!(
            "{}:{}",
            self.host_path.display(),
            self.container_path.display()
        );
        if self.read_only {
            format!("{base}:ro")
        } else {
            base
        }
    }
}

/// Materializes [`Mount`]s from the file store into a host-side staging
/// directory and returns Docker bind mount specifications.
///
/// Files are staged under `<staging_root>/<first-2-hex>/<content_id>` using
/// the same layout as [`awaken_file_store::FileStore`] to deduplicate blobs
/// that are shared across multiple containers.  Writes are atomic
/// (temp-file + rename) so a partially-written blob is never visible.
pub struct DockerMountMaterializer {
    store: Arc<FileStore>,
    staging_root: PathBuf,
}

impl DockerMountMaterializer {
    /// Create a materializer backed by `store`, staging blobs under `staging_root`.
    pub fn new(store: Arc<FileStore>, staging_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            staging_root: staging_root.into(),
        }
    }

    /// Materialize `mounts` into the staging directory and return one
    /// [`DockerBind`] per mount.  Mounts are processed sequentially; the
    /// first error aborts the batch.
    pub async fn materialize(&self, mounts: &[Mount]) -> Result<Vec<DockerBind>, SandboxError> {
        let mut binds = Vec::with_capacity(mounts.len());
        for mount in mounts {
            binds.push(self.materialize_one(mount).await?);
        }
        Ok(binds)
    }

    /// Materialize a single [`Mount`] and return its [`DockerBind`].
    ///
    /// Only `FileStore` sources are supported; `Secret` sources must be
    /// resolved by the provider before Docker materialization.
    pub async fn materialize_one(&self, mount: &Mount) -> Result<DockerBind, SandboxError> {
        let content_id = match &mount.source {
            MountSource::FileStore { content_id } => content_id,
            MountSource::Secret { reference } => {
                return Err(SandboxError::UnresolvedSecret {
                    reference: reference.clone(),
                });
            }
        };

        let blob = self.store.get(content_id).await?;

        // Staging path mirrors the file-store layout so the same blob is
        // de-duplicated across containers sharing the same staging root.
        let staged = self.staging_root.join(&content_id[..2]).join(content_id);

        if !staged.exists() {
            if let Some(parent) = staged.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Atomic write: write to a temp file then rename.
            let tmp = staged.with_extension("tmp");
            tokio::fs::write(&tmp, &blob).await?;
            tokio::fs::rename(&tmp, &staged).await?;
        }

        Ok(DockerBind {
            host_path: staged,
            container_path: mount.target.clone(),
            read_only: mount.access == MountAccess::ReadOnly,
        })
    }
}
