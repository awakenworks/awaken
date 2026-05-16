//! Storage backend implementations for the awaken framework.
//!
//! Provides concrete implementations of the storage traits defined in
//! `awaken-contract`: [`ThreadStore`](awaken_contract::contract::storage::ThreadStore),
//! [`RunStore`](awaken_contract::contract::storage::RunStore),
//! [`ThreadRunStore`](awaken_contract::contract::storage::ThreadRunStore),
//! [`ProfileStore`](awaken_contract::contract::profile_store::ProfileStore),
//! [`ConfigStore`](awaken_contract::contract::config_store::ConfigStore), and
//! [`MailboxStore`](awaken_contract::contract::mailbox::MailboxStore).

pub mod memory;
pub mod memory_mailbox;

/// Wall-clock time in milliseconds since the UNIX epoch.
///
/// Panics if the system clock is set before 1970 — a severely misconfigured
/// system that cannot be meaningfully recovered from.
pub(crate) fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

#[cfg(feature = "file")]
pub mod file;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "sqlite")]
pub mod sqlite_mailbox;

#[cfg(feature = "nats")]
mod nats_keys;

#[cfg(feature = "nats")]
mod nats_connect;

#[cfg(feature = "nats")]
pub mod nats_mailbox;

#[cfg(feature = "nats")]
pub mod nats_buffered_thread;

pub use memory::InMemoryStore;
pub use memory_mailbox::InMemoryMailboxStore;

#[cfg(feature = "file")]
pub use file::FileStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;

#[cfg(feature = "sqlite")]
pub use sqlite_mailbox::SqliteMailboxStore;

#[cfg(feature = "nats")]
pub use nats_mailbox::{NatsMailboxConfig, NatsMailboxStore};

#[cfg(feature = "nats")]
pub use nats_buffered_thread::{
    NatsBufferedThreadConfig, NatsBufferedThreadStore, ReadConsistency,
};
