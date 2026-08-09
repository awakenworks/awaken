//! Durable adapters for the Model Catalog repository port.

#![forbid(unsafe_code)]

#[cfg(feature = "postgres")]
pub mod postgres;
mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::PostgresCatalogRepo;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteCatalogRepo;
