//! Storage backend implementations for the awaken framework.
//!
//! Provides concrete implementations of the storage traits defined in
//! `awaken-contract`: [`ThreadStore`], [`RunStore`], [`MailboxStore`],
//! and [`ThreadRunStore`].

pub mod memory;

#[cfg(feature = "file")]
pub mod file;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "nats")]
pub mod nats;

pub use memory::InMemoryStore;

#[cfg(feature = "file")]
pub use file::FileStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;

#[cfg(feature = "nats")]
pub use nats::NatsBufferedWriter;
