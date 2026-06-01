// ADR-0038 D7: this crate hosts ThreadRunStore impls and coordinator-internal
// callers that legitimately invoke the deprecated checkpoint primitive.
#![allow(deprecated)]

//! Storage backend implementations for the awaken framework.
//!
//! Provides concrete implementations of the storage traits defined in
//! `awaken-contract`: [`ThreadStore`](awaken_server_contract::contract::storage::ThreadStore),
//! [`RunStore`](awaken_server_contract::contract::storage::RunStore),
//! [`ThreadRunStore`](awaken_server_contract::contract::storage::ThreadRunStore),
//! [`ProfileStore`](awaken_server_contract::contract::profile_store::ProfileStore),
//! [`ConfigStore`](awaken_server_contract::contract::config_store::ConfigStore), and
//! [`MailboxStore`](awaken_server_contract::contract::mailbox::MailboxStore).

mod commit_batch;
mod mailbox_state;
pub mod memory;
pub mod memory_commit_coordinator;
pub mod memory_event_store;
pub mod memory_mailbox;
pub mod memory_outbox;
pub mod memory_protocol_replay_log;
pub mod memory_versioned_registry;
mod message_validation;
pub mod pending_message_store;

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

#[cfg(feature = "file")]
mod file_commit_coordinator;

#[cfg(feature = "file")]
mod file_versioned_registry;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
mod postgres_commit_coordinator;

#[cfg(feature = "postgres")]
mod postgres_event;

#[cfg(feature = "postgres")]
mod postgres_event_subscriber;

#[cfg(feature = "postgres")]
mod postgres_event_lookup;

#[cfg(feature = "postgres")]
mod postgres_protocol_replay;

#[cfg(feature = "postgres")]
mod postgres_outbox;

#[cfg(feature = "postgres")]
mod postgres_stream_checkpoint;

#[cfg(feature = "postgres")]
mod postgres_versioned_registry;

#[cfg(feature = "postgres")]
mod postgres_versioned_registry_schema;

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
pub use memory_commit_coordinator::MemoryCommitCoordinator;
pub use memory_event_store::InMemoryEventStore;
pub use memory_mailbox::InMemoryMailboxStore;
pub use memory_outbox::InMemoryOutboxStore;
pub use memory_protocol_replay_log::InMemoryProtocolReplayLog;
pub use memory_versioned_registry::InMemoryVersionedRegistryStore;
pub use pending_message_store::{PendingMessageStore, PendingThreadRunStore};

#[cfg(feature = "file")]
pub use file::FileStore;

#[cfg(feature = "file")]
pub use file_commit_coordinator::FileCommitCoordinator;

#[cfg(feature = "file")]
pub use file_versioned_registry::FileVersionedRegistryStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;

#[cfg(feature = "postgres")]
pub use postgres_commit_coordinator::PgCommitCoordinator;

#[cfg(feature = "postgres")]
pub use postgres_outbox::enqueue_outbox_in_transaction;

#[cfg(feature = "sqlite")]
pub use sqlite_mailbox::SqliteMailboxStore;

#[cfg(feature = "nats")]
pub use nats_mailbox::{NatsMailboxConfig, NatsMailboxStore};

#[cfg(feature = "nats")]
pub use nats_buffered_thread::{
    NatsBufferedThreadConfig, NatsBufferedThreadStore, ReadConsistency,
};
