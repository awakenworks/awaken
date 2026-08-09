//! Durable SQLite and Postgres adapters for the Agent Config repository ports.

#![forbid(unsafe_code)]

mod postgres;
mod schema;
mod sqlite;

pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};
