//! Durable adapters for the Control-owned data-subject application.
//!
//! Aggregate rules, repository ports, consent, enrollment, and erasure decisions
//! live in `awaken-data-subject-application`; this crate only implements storage.

#[cfg(feature = "postgres")]
mod postgres;
mod schema;
mod sqlite;

#[cfg(any(test, feature = "test-support"))]
mod memory;

#[cfg(feature = "postgres")]
pub use postgres::{PgDataSubjectRepo, PgStoreError};
pub use schema::{CONTROL_BUNDLE_ID, CONTROL_PREFIX, control_data_subject_bundle};
pub use sqlite::{SqliteDataSubjectRepo, StoreError};

#[cfg(any(test, feature = "test-support"))]
pub use memory::InMemoryDataSubjectRepo;
