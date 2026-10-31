//! Durable adapters for the Model Catalog repository port.

#![forbid(unsafe_code)]
#![expect(
    unreachable_pub,
    reason = "pending: narrow the unreachable items to pub(crate)"
)]

#[cfg(feature = "postgres")]
pub mod postgres;
mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::PostgresCatalogRepo;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteCatalogRepo;
