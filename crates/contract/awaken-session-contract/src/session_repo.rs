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

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_tenancy::ScopeId;
use serde_json::Value;

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

    /// Persist under the self-hosted default scope. Production request paths with
    /// an edge-resolved owner use [`Self::save_owned`] directly; this convenience
    /// keeps scope-free local/test callers deterministic without reintroducing a
    /// second owner write.
    async fn save(&self, session: PersistedSession) {
        self.save_owned("default", session).await;
    }

    /// The stored configuration for `session_id`, if any.
    async fn get(&self, session_id: &str) -> Option<PersistedSession>;

    /// The atomically persisted owner scope of `session_id`, if the row exists.
    async fn owner(&self, _session_id: &str) -> Option<String> {
        None
    }
}

// The in-memory reference backends (plain + scoped) live outward in
// `awaken-session-store`, beside the durable sqlite/postgres siblings.

// --- Tenant isolation: the ScopedRepo decorator (ADR-0051 D3) ---------------

/// The scope-aware backing store — the infrastructure-facing half of the port.
/// Every method carries the [`ScopeId`], so a concrete store persists it as one
/// opaque `scope_id` column beside the serialized session and filters reads by it.
/// The core never sees this trait; it holds the scope-free
/// [`ManagedSessionRepository`], which [`ScopedSessionRepo`] implements by binding
/// a scope.
#[async_trait]
pub trait ScopedSessionStore: Send + Sync {
    /// Upsert `session` under `scope` (idempotent by `(scope, session_id)`).
    async fn save_scoped(&self, scope: &ScopeId, session: PersistedSession);

    /// The session for `session_id` **within `scope`** — a row owned by another
    /// scope is invisible (the isolation fence), so this returns `None` for it.
    async fn get_scoped(&self, scope: &ScopeId, session_id: &str) -> Option<PersistedSession>;
}

/// The decorator that makes tenancy an edge aspect: it implements the scope-free
/// [`ManagedSessionRepository`] the core holds by binding one [`ScopeId`] and
/// delegating to a [`ScopedSessionStore`]. Constructed at the edge from the
/// request's resolved scope, so the runtime cannot pass or read a scope — every
/// write auto-stamps the bound scope and every read auto-filters by it, and there
/// is no scope argument for a call site to forget.
pub struct ScopedSessionRepo<S: ScopedSessionStore> {
    inner: Arc<S>,
    scope: ScopeId,
}

impl<S: ScopedSessionStore> ScopedSessionRepo<S> {
    /// Bind `store` to `scope` for one tenant's requests.
    pub fn new(store: Arc<S>, scope: ScopeId) -> Self {
        Self {
            inner: store,
            scope,
        }
    }

    /// The scope this repository is bound to.
    #[must_use]
    pub fn scope(&self) -> &ScopeId {
        &self.scope
    }
}

#[async_trait]
impl<S: ScopedSessionStore> ManagedSessionRepository for ScopedSessionRepo<S> {
    async fn save_owned(&self, _owner_scope: &str, session: PersistedSession) {
        self.inner.save_scoped(&self.scope, session).await;
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        self.inner.get_scoped(&self.scope, session_id).await
    }
}
