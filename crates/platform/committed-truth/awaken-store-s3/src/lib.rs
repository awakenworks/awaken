//! Session-scoped, conditional object-store publication and immutable history.
mod bound;
mod config;
pub use config::S3Config;
mod canonical;
mod history;
mod maintenance;
mod model;
mod objects;
mod ports;
mod progress;
mod projection;
mod session;
mod session_repository;
mod stream_archive;
mod stream_index;
mod streams;

pub use bound::{ClaimedSession, SessionProjection};
pub use canonical::{CanonicalCheckpoints, CanonicalRunCommits, ReadOnlyCheckpoints};
pub use maintenance::GcReport;
pub use model::{
    BlobRef, Claim, CommitDescriptor, HeadView, Limits, ObjectRef, ReadCursor, RunPhase,
    SessionKey, StreamBatch, StreamCursor, StreamReadCursor, StreamReadPage, StreamSubscription,
    Subscription, ThreadView,
};
pub use objects::{RequestCounts, S3Storage};
pub use progress::SessionProgressPublisher;
pub use projection::CommittedProjection;
pub use session::SessionStorage;
pub use session_repository::S3ComponentStore;
pub use streams::{BoundCheckpoints, SessionStreams};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Admission(
        #[from] awaken_agent_contract::thread::commit::operation::CommitOperationAdmissionError,
    ),
    #[error("operation identity mismatch: {0}")]
    Identity(String),
    #[error("storage unavailable: {0}")]
    Storage(String),
    #[error("publication outcome unknown: {0}")]
    OutcomeUnknown(String),
    #[error("Session or Thread was not found")]
    NotFound,
    #[error("claim is fenced")]
    Fenced,
    #[error("storage conflict: {0}")]
    Conflict(String),
    #[error("invalid storage request: {0}")]
    Invalid(String),
    #[error("corrupt storage object: {0}")]
    Corrupt(String),
    #[error("storage capacity exceeded: {0}")]
    Capacity(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(feature = "test-support")]
pub mod test_support {
    /// The real engine over an isolated deterministic object transport.
    pub fn storage(
        prefix: &str,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> super::S3Storage {
        super::S3Storage::new(
            std::sync::Arc::new(object_store::memory::InMemory::new()),
            prefix,
            clock,
            Default::default(),
        )
        .expect("isolated object fixture configuration")
    }
}

mod stream_read;
