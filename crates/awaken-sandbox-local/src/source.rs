use std::path::PathBuf;

use bytes::Bytes;

/// Origin of content to be ingested into the file store.
#[derive(Debug, Clone)]
pub enum Source {
    /// Read content from a local filesystem path.
    File(PathBuf),
    /// Use the provided bytes directly.
    Bytes(Bytes),
}
