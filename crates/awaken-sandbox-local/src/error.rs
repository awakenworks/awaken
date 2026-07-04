use awaken_file_store::FileStoreError;

/// Errors produced by sandbox provider operations.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("file store error: {0}")]
    FileStore(#[from] FileStoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("source path does not exist: {path}")]
    SourceNotFound { path: std::path::PathBuf },

    #[error("mount target path escapes sandbox: {path}")]
    PathEscape { path: std::path::PathBuf },

    #[error("tar archive error: {0}")]
    Tar(String),
}
