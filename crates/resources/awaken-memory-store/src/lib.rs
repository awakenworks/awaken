//! Durable memory persistence for the resources plane.
//!
//! The [`MemoryRepository`] port with durable SQLite and Postgres backends plus an
//! explicit `test-support` volatile backend. A store is addressed only by an opaque,
//! globally unique id; workspace ownership and authorization deliberately remain
//! outside this storage adapter in the resource registry and authorization edge.
//!
//! Recall, extraction, API history, and mounts all use this same store-scoped
//! aggregate; there is no Host-global extraction directory.

/// Path-addressed, CAS memory model (ADR-0053): the `MemoryRepository` port a write-through
/// FUSE mount projects.
pub mod repository;

pub use awaken_resource_contract::memory_store_stem as sanitize_stem;
#[cfg(any(test, feature = "test-support"))]
pub use repository::VolatileMemoryRepository;
pub use repository::{
    MAX_MEMORIES_PER_STORE, MAX_MEMORY_BYTES, MemErr, Memory, MemoryEntry, MemoryPurgeSummary,
    MemoryRepository, MemoryVersion, MemoryVersionOperation, sha256_hex,
};

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PgStoreError, PostgresMemoryRepository};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use schema::{BUNDLE_ID, memory_store_bundle};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteMemoryRepository, StoreError};
