//! Storage backend implementations for the awaken framework.
//!
//! Provides concrete implementations of the storage traits defined in
//! `awaken-contract`: [`ThreadStore`], [`RunStore`], [`ThreadRunStore`],
//! and [`MailboxStore`](awaken_contract::MailboxStore).

pub mod memory;
pub mod memory_mailbox;

#[cfg(feature = "file")]
pub mod file;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "sqlite")]
pub mod sqlite_mailbox;

pub use memory::InMemoryStore;
pub use memory_mailbox::InMemoryMailboxStore;

#[cfg(feature = "file")]
pub use file::FileStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;

#[cfg(feature = "sqlite")]
pub use sqlite_mailbox::SqliteMailboxStore;
