//! Server and store boundary contracts for Awaken.
//!
//! This crate names the server-facing contract surface. It re-exports runtime
//! vocabulary and owns the server/store scope boundary types and scoped store
//! wrappers.

#![allow(missing_docs)]

pub mod contract;

pub use awaken_runtime_contract::*;
pub use contract::audit_log::*;
pub use contract::config_store::ScopedConfigStore;
pub use contract::config_store::*;
pub use contract::mailbox::ScopedMailboxStore;
pub use contract::mailbox::*;
pub use contract::outbox::ScopedOutboxStore;
pub use contract::outbox::*;
pub use contract::protocol_replay_log::ScopedProtocolReplayLog;
pub use contract::protocol_replay_log::*;
pub use contract::registry_graph::*;
pub use contract::scope::{
    DEFAULT_SCOPE_ID, RequestSurface, ScopeContext, ScopeError, ScopeId, scoped_key, unscoped_key,
};
pub use contract::storage::ScopedThreadRunStore;
pub use contract::versioned_registry::*;
pub use contract::versioned_registry::{ScopedVersionedRegistry, TypedVersionedRegistry};

pub mod runtime {
    pub use awaken_runtime_contract::*;
}

#[cfg(test)]
mod tests;
