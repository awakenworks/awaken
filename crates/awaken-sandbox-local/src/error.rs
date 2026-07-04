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

    #[error("no secret broker configured — attach one with `with_broker`")]
    NoBroker,

    #[error("secret broker error for `{reference}`: {message}")]
    Broker { reference: String, message: String },

    #[error("secret mount `{reference}` must be resolved before use with this materializer")]
    UnresolvedSecret { reference: String },
}
