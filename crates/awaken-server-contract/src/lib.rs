//! Server and store boundary contracts for Awaken.
//!
//! This crate names the server-facing contract surface. It owns the
//! server/store scope boundary types and scoped store wrappers, and it
//! deliberately re-exports the full runtime vocabulary so that server/store
//! code can import everything it needs from a single crate. The re-export is a
//! superset, not a firewall: a consumer cannot tell from the import path alone
//! whether a type is server-only or shared with the runtime. That ergonomic
//! trade-off is intentional — narrowing it would force every server/store
//! crate to depend on both contracts and split its imports.

#![allow(missing_docs)]

pub mod contract;

// Superset re-export of the runtime contract surface (see crate docs).
pub use awaken_runtime_contract::*;

// Server/store-owned surfaces. Glob re-exports cover the `Scoped*` wrappers; do
// not add explicit re-exports for items the glob already brings in.
pub use contract::audit_log::*;
pub use contract::config_store::*;
pub use contract::event_store::*;
pub use contract::mailbox::*;
pub use contract::outbox::*;
pub use contract::protocol_replay_log::*;
pub use contract::registry_graph::*;
pub use contract::scope::{
    DEFAULT_SCOPE_ID, RequestSurface, ScopeContext, ScopeError, ScopeId, scoped_key, unscoped_key,
};
pub use contract::staged_commit::{CheckpointStagedWrites, StagedCommitCoordinator};
pub use contract::storage::ScopedThreadRunStore;
pub use contract::versioned_registry::*;

#[cfg(test)]
mod tests;
