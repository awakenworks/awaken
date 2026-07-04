use std::path::PathBuf;

use awaken_file_store::ContentId;

/// Whether the mount is read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

/// How long a secret mount's resolved content is retained.
///
/// Non-secret (`FileStore`) mounts are always `PerRun`; the lifetime field
/// only affects write-back decisions for `Secret` sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountLifetime {
    /// Content is discarded when the sandbox is dropped.
    #[default]
    PerRun,
    /// Provider reads back the (possibly refreshed) file after the run and
    /// writes it to the broker.  Only meaningful for `Secret` + `ReadWrite`.
    Durable,
}

/// Where a mount's content originates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A content-addressed blob from the file store.
    FileStore { content_id: ContentId },
    /// A credential resolved by the secret broker at materialization time.
    ///
    /// Only the opaque `reference` string crosses the contract seam — never
    /// the secret bytes themselves.  A `ReadWrite + Durable` mount triggers
    /// write-back after the run via [`SecretBroker::write_back`].
    ///
    /// [`SecretBroker::write_back`]: crate::SecretBroker::write_back
    Secret { reference: String },
}

/// A file to materialize inside a sandbox.
///
/// For `FileStore` sources, `content_id` is used to look up the blob from
/// the file store.  For `Secret` sources, the provider resolves the reference
/// via a [`SecretBroker`] and writes the bytes to `target`.
///
/// [`SecretBroker`]: crate::SecretBroker
#[derive(Debug, Clone)]
pub struct Mount {
    /// Content origin.
    pub source: MountSource,
    /// Relative path inside the sandbox root (or the container path for
    /// Docker/K8s).
    pub target: PathBuf,
    /// Whether the bind mount should be read-only (default) or read-write.
    pub access: MountAccess,
    /// Lifetime of the mount (relevant for secret write-back).
    pub lifetime: MountLifetime,
}

impl Mount {
    /// Create a read-only, per-run file-store mount.
    pub fn new(content_id: impl Into<ContentId>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: MountSource::FileStore {
                content_id: content_id.into(),
            },
            target: target.into(),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
        }
    }

    /// Create a read-write, per-run file-store mount.
    pub fn read_write(content_id: impl Into<ContentId>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: MountSource::FileStore {
                content_id: content_id.into(),
            },
            target: target.into(),
            access: MountAccess::ReadWrite,
            lifetime: MountLifetime::PerRun,
        }
    }

    /// Create a read-only, per-run secret mount.
    pub fn secret(reference: impl Into<String>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: MountSource::Secret {
                reference: reference.into(),
            },
            target: target.into(),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
        }
    }

    /// Override access mode.
    pub fn with_access(mut self, access: MountAccess) -> Self {
        self.access = access;
        self
    }

    /// Override lifetime (relevant for secret write-back).
    pub fn with_lifetime(mut self, lifetime: MountLifetime) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// A durable, writable secret mount triggers write-back after the run.
    ///
    /// Returns `true` only for `Secret` sources that are both `ReadWrite`
    /// and `Durable` — the combination that signals the agent may refresh its
    /// own auth file and the provider should persist the update.
    #[must_use]
    pub fn is_secret_writeback(&self) -> bool {
        matches!(self.source, MountSource::Secret { .. })
            && self.access == MountAccess::ReadWrite
            && self.lifetime == MountLifetime::Durable
    }
}
