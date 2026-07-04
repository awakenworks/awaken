use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_file_store::{ContentId, FileStore};

use crate::error::SandboxError;
use crate::mount::Mount;
use crate::sandbox::Sandbox;
use crate::source::Source;

/// Trait implemented by every sandbox provider.
///
/// All methods are `async`; implementors must not block the executor.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// Ingest a [`Source`] into the backing file store and return its
    /// [`ContentId`].
    async fn resolve_source(&self, source: &Source) -> Result<ContentId, SandboxError>;

    /// Materialize a single [`Mount`] inside an existing `sandbox`.
    ///
    /// Creates parent directories as needed. Returns an error if `mount.target`
    /// would escape the sandbox root.
    async fn realize_mount(&self, sandbox: &Sandbox, mount: &Mount) -> Result<(), SandboxError>;

    /// Create a fresh [`Sandbox`], resolve all mounts, and return it.
    async fn create_sandbox(&self, mounts: &[Mount]) -> Result<Sandbox, SandboxError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ──────────────────────────────────────────────────────────────────────────────

async fn ingest_source(store: &FileStore, source: &Source) -> Result<ContentId, SandboxError> {
    let data: Vec<u8> = match source {
        Source::File(path) => {
            if !path.exists() {
                return Err(SandboxError::SourceNotFound { path: path.clone() });
            }
            tokio::fs::read(path).await?
        }
        Source::Bytes(bytes) => bytes.to_vec(),
    };
    Ok(store.put(&data).await?)
}

async fn materialize_mount(
    store: &FileStore,
    sandbox_root: &Path,
    mount: &Mount,
) -> Result<(), SandboxError> {
    // Reject paths that escape the sandbox root.
    let target = sandbox_root.join(&mount.target);
    let canonical_root = sandbox_root.canonicalize()?;
    // The target may not yet exist; walk ancestors to find a canonical prefix.
    let canonical_target = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // Resolve the longest existing prefix, then append the rest.
            let mut existing = target.as_path();
            loop {
                if existing.exists() {
                    break existing.canonicalize()?;
                }
                existing = existing.parent().unwrap_or_else(|| Path::new("/"));
            }
        }
    };
    if !canonical_target.starts_with(&canonical_root) {
        return Err(SandboxError::PathEscape {
            path: mount.target.clone(),
        });
    }

    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let blob = store.get(&mount.content_id).await?;
    tokio::fs::write(&target, blob).await?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// LocalSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

/// Sandbox provider backed directly by a [`FileStore`].
///
/// Each call to [`create_sandbox`][SandboxProvider::create_sandbox] creates a
/// fresh [`tempfile::TempDir`] and materializes the requested mounts into it.
pub struct LocalSandboxProvider {
    store: Arc<FileStore>,
}

impl LocalSandboxProvider {
    pub fn new(store: Arc<FileStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SandboxProvider for LocalSandboxProvider {
    async fn resolve_source(&self, source: &Source) -> Result<ContentId, SandboxError> {
        ingest_source(&self.store, source).await
    }

    async fn realize_mount(&self, sandbox: &Sandbox, mount: &Mount) -> Result<(), SandboxError> {
        materialize_mount(&self.store, sandbox.path(), mount).await
    }

    async fn create_sandbox(&self, mounts: &[Mount]) -> Result<Sandbox, SandboxError> {
        let dir = tempfile::TempDir::new()?;
        let sandbox = Sandbox::new(dir);
        for mount in mounts {
            self.realize_mount(&sandbox, mount).await?;
        }
        Ok(sandbox)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// NamespaceSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

/// Sandbox provider that scopes sandbox roots under a named namespace directory.
///
/// All sandboxes created by this provider live under
/// `<namespace_root>/<namespace>/`. This provides lightweight directory-level
/// isolation between tenants or runs without requiring OS-level namespace
/// features.
pub struct NamespaceSandboxProvider {
    store: Arc<FileStore>,
    namespace: String,
    namespace_root: std::path::PathBuf,
}

impl NamespaceSandboxProvider {
    /// Create a provider scoped to `namespace` inside `namespace_root`.
    ///
    /// The `namespace_root/<namespace>` directory is created on demand.
    pub fn new(
        store: Arc<FileStore>,
        namespace: impl Into<String>,
        namespace_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            store,
            namespace: namespace.into(),
            namespace_root: namespace_root.into(),
        }
    }

    async fn sandbox_parent(&self) -> Result<std::path::PathBuf, SandboxError> {
        let parent = self.namespace_root.join(&self.namespace);
        tokio::fs::create_dir_all(&parent).await?;
        Ok(parent)
    }
}

#[async_trait]
impl SandboxProvider for NamespaceSandboxProvider {
    async fn resolve_source(&self, source: &Source) -> Result<ContentId, SandboxError> {
        ingest_source(&self.store, source).await
    }

    async fn realize_mount(&self, sandbox: &Sandbox, mount: &Mount) -> Result<(), SandboxError> {
        materialize_mount(&self.store, sandbox.path(), mount).await
    }

    async fn create_sandbox(&self, mounts: &[Mount]) -> Result<Sandbox, SandboxError> {
        let parent = self.sandbox_parent().await?;
        let dir = tempfile::Builder::new()
            .prefix("sandbox-")
            .tempdir_in(&parent)?;
        let sandbox = Sandbox::new(dir);
        for mount in mounts {
            self.realize_mount(&sandbox, mount).await?;
        }
        Ok(sandbox)
    }
}
