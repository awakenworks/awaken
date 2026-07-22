//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port: MCP entries keep only the wire-echo
//! `{name, type, url}` values, never a credential — those are re-materialized from
//! the vault at prepare time (G3).

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::SessionLifecycleFact;

/// The durable, adapter-side configuration of one Managed session, keyed by its
/// id (which is also its thread id). Everything here is what the wire `Session`
/// object needs beyond the runtime's committed transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedSession {
    pub session_id: String,
    pub agent_id: String,
    /// The resolved model the session runs (the request override, else the host
    /// default at create time).
    pub model: String,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub environment_id: String,
    /// The accepted MCP servers in the SDK wire shape (`{name, type, url}`) — the
    /// echo the agent object reports; never a credential.
    pub mcp_servers: Vec<Value>,
    /// Durable resource activation state. Its `active` manifest is the exact,
    /// secret-free Session pin; `pending` and activation records make external
    /// realization/release recoverable without importing authorization concepts.
    pub resources: crate::SessionResourceState,
    /// Durable lifecycle projection used when a process rehydrates the session.
    pub status: String,
    pub archived_at: Option<String>,
}

/// One durable Session together with its intrinsic Workspace partition.
///
/// Recovery consumes this envelope atomically instead of looking up an owner in
/// a second step. It contains no principal, role, policy, credential, or
/// authorization decision; `workspace_id` is resource routing state only.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedPersistedSession {
    pub workspace_id: String,
    pub session: PersistedSession,
}

/// The port the Managed adapter drives to persist and restore [`PersistedSession`]
/// rows. The default in-memory impl keeps single-process behavior; a durable impl
/// (e.g. SQLite alongside the transcript store) lets a session survive a restart
/// and be reported faithfully by another process.
#[async_trait]
pub trait ManagedSessionRepository: Send + Sync {
    /// Persist a session and its owning scope in one repository operation.
    ///
    /// The owner is part of the adapter-side persistence envelope rather than the
    /// tenancy-agnostic [`PersistedSession`] value. Keeping both arguments on one
    /// required method makes the crash invariant explicit: a visible session row
    /// can never exist without its ownership fence.
    async fn save_owned(&self, owner_scope: &str, session: PersistedSession);

    /// Atomically persist the session, owner fence, and lifecycle outbox fact.
    async fn save_owned_with_lifecycle(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        fact: SessionLifecycleFact,
    );

    /// Commit a lifecycle transition fact idempotently by stable id.
    async fn append_lifecycle(&self, fact: SessionLifecycleFact);

    /// Atomically mark the durable session terminated and commit its fact.
    async fn archive_with_lifecycle(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: SessionLifecycleFact,
    );

    /// Atomically delete the session row and commit its terminal fact.
    async fn delete_with_lifecycle(&self, session_id: &str, fact: SessionLifecycleFact);

    async fn pending_lifecycle(&self) -> Vec<SessionLifecycleFact>;

    async fn complete_lifecycle(&self, fact_id: &str);

    /// Persist under the self-hosted default scope. Production request paths with
    /// an edge-resolved owner use [`Self::save_owned`] directly; this convenience
    /// keeps scope-free local/test callers deterministic without reintroducing a
    /// second owner write.
    async fn save(&self, session: PersistedSession) {
        self.save_owned("default", session).await;
    }

    /// The stored configuration for `session_id`, if any.
    async fn get(&self, session_id: &str) -> Option<PersistedSession>;

    /// Sessions with a Prepared/Releasing resource transition. Implementations
    /// must preserve their ordinary tenancy fence; the coordinator obtains the
    /// already-trusted owner separately through [`Self::owner`].
    async fn pending_resource_sessions(&self) -> Vec<ScopedPersistedSession> {
        Vec::new()
    }

    /// The atomically persisted owner scope of `session_id`, if the row exists.
    async fn owner(&self, _session_id: &str) -> Option<String> {
        None
    }
}

// In-memory and durable adapters live outward in `awaken-session-store`.
// Workspace ownership is persisted atomically beside each row through
// `save_owned*`; authorization scope decorators do not belong in this resource
// persistence port.
