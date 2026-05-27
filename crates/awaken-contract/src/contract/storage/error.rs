use thiserror::Error;

/// Errors returned by storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The provided input violates a storage-level invariant.
    #[error("validation error: {0}")]
    Validation(String),
    /// The requested entity was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// An entity with the given key already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// Optimistic concurrency conflict.
    #[error("version conflict: expected {expected}, actual {actual}")]
    VersionConflict {
        /// The version the caller expected.
        expected: u64,
        /// The actual current version.
        actual: u64,
    },
    /// Pending freeze selected a different set of pending ids than the caller
    /// prepared against.
    #[error("pending selection conflict: expected {expected_ids:?}, actual {actual_ids:?}")]
    PendingSelectionConflict {
        /// Pending ids selected by the caller before attempting freeze.
        expected_ids: Vec<String>,
        /// Pending ids selected inside the backend transaction.
        actual_ids: Vec<String>,
    },
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(String),
    /// The operation may have committed durably, but the caller cannot know
    /// whether follow-up promotion/cache work completed.
    #[error("commit outcome unknown: {0}")]
    CommitUnknown(String),
    /// A serialization or deserialization error occurred.
    #[error("serialization error: {0}")]
    Serialization(String),
}
