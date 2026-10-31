//! Coordinator-owned captured runtime content persistence.
//!
//! Consent and erasure-job state remain in Control's data-subject application/store;
//! this crate owns only capture records, restriction state, TTL, and subject
//! erasure at the Coordinator boundary.

#[cfg(any(test, feature = "test-support"))]
mod capture_store;
#[cfg(feature = "postgres")]
mod postgres;
mod schema;
mod sqlite;

#[cfg(any(test, feature = "test-support"))]
pub use capture_store::{CapturedRecord, InMemoryCapturedContentStore};
#[cfg(feature = "postgres")]
pub use postgres::PgCapturedContentStore;
pub use schema::{
    COORDINATOR_CAPTURE_BUNDLE_ID, COORDINATOR_CAPTURE_PREFIX, coordinator_data_capture_bundle,
};
pub use sqlite::SqliteCapturedContentStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

#[cfg(feature = "postgres")]
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("schema: {0}")]
    Schema(String),
}
