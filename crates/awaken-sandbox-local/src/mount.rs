use std::path::PathBuf;

use awaken_file_store::ContentId;

/// Whether the mount is read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

/// A file to materialize inside a sandbox.
///
/// `content_id` identifies the blob in the file store; `target` is the
/// relative path within the sandbox root where the file will be written.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Content-addressed identifier of the blob to materialize.
    pub content_id: ContentId,
    /// Relative path inside the sandbox root (or the container path for Docker/K8s).
    pub target: PathBuf,
    /// Whether the bind mount should be read-only (default) or read-write.
    pub access: MountAccess,
}

impl Mount {
    pub fn new(content_id: impl Into<ContentId>, target: impl Into<PathBuf>) -> Self {
        Self {
            content_id: content_id.into(),
            target: target.into(),
            access: MountAccess::ReadOnly,
        }
    }

    pub fn read_write(content_id: impl Into<ContentId>, target: impl Into<PathBuf>) -> Self {
        Self {
            content_id: content_id.into(),
            target: target.into(),
            access: MountAccess::ReadWrite,
        }
    }

    pub fn with_access(mut self, access: MountAccess) -> Self {
        self.access = access;
        self
    }
}
