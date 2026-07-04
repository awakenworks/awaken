/// Errors returned by [`FileStore`][crate::FileStore] operations.
#[derive(Debug, thiserror::Error)]
pub enum FileStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("content not found: {id}")]
    NotFound { id: String },

    #[error("invalid content id (expected 64 hex chars): {0}")]
    InvalidId(String),
}
