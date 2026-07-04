use std::path::PathBuf;

use awaken_file_store::ContentId;

/// A file to materialize inside a sandbox.
///
/// `content_id` identifies the blob in the file store; `target` is the
/// relative path within the sandbox root where the file will be written.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Content-addressed identifier of the blob to materialize.
    pub content_id: ContentId,
    /// Relative path inside the sandbox root.
    pub target: PathBuf,
}

impl Mount {
    pub fn new(content_id: impl Into<ContentId>, target: impl Into<PathBuf>) -> Self {
        Self {
            content_id: content_id.into(),
            target: target.into(),
        }
    }
}
