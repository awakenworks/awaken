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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::Mutex;

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
    /// Persist (idempotent upsert by `session_id`) a session's configuration.
    async fn save(&self, session: PersistedSession);

    /// The stored configuration for `session_id`, if any.
    async fn get(&self, session_id: &str) -> Option<PersistedSession>;
}

/// In-memory session repository (the default): keeps rows for the process's
/// lifetime. Restart or a second process starts empty, so rehydration falls back
/// to committed truth alone — the same behavior as before this port existed.
#[derive(Default)]
pub struct InMemorySessionRepository {
    rows: Mutex<HashMap<String, PersistedSession>>,
}

#[async_trait]
impl ManagedSessionRepository for InMemorySessionRepository {
    async fn save(&self, session: PersistedSession) {
        self.rows
            .lock()
            .expect("session repo mutex poisoned")
            .insert(session.session_id.clone(), session);
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        self.rows
            .lock()
            .expect("session repo mutex poisoned")
            .get(session_id)
            .cloned()
    }
}

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
    async fn save(&self, session: PersistedSession) {
        self.inner.save_scoped(&self.scope, session).await;
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        self.inner.get_scoped(&self.scope, session_id).await
    }
}

/// In-memory [`ScopedSessionStore`] keyed by `(scope, session_id)` — the executable
/// definition of the isolation fence, used by tests and the single-process
/// scoped default with zero durable dependencies.
#[derive(Default)]
pub struct InMemoryScopedSessionStore {
    rows: Mutex<HashMap<(String, String), PersistedSession>>,
}

impl InMemoryScopedSessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ScopedSessionStore for InMemoryScopedSessionStore {
    async fn save_scoped(&self, scope: &ScopeId, session: PersistedSession) {
        self.rows
            .lock()
            .expect("scoped session store mutex poisoned")
            .insert((scope.0.clone(), session.session_id.clone()), session);
    }

    async fn get_scoped(&self, scope: &ScopeId, session_id: &str) -> Option<PersistedSession> {
        self.rows
            .lock()
            .expect("scoped session store mutex poisoned")
            .get(&(scope.0.clone(), session_id.to_string()))
            .cloned()
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;

    fn session(id: &str) -> PersistedSession {
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "assistant".into(),
            model: "kimi".into(),
            title: None,
            metadata: BTreeMap::new(),
            environment_id: "env".into(),
            mcp_servers: Vec::new(),
        }
    }

    fn repo(
        store: &Arc<InMemoryScopedSessionStore>,
        scope: &str,
    ) -> ScopedSessionRepo<InMemoryScopedSessionStore> {
        ScopedSessionRepo::new(store.clone(), ScopeId::from(scope))
    }

    #[tokio::test]
    async fn a_scope_reads_back_its_own_write() {
        let store = Arc::new(InMemoryScopedSessionStore::new());
        let a = repo(&store, "ws_a");
        a.save(session("s1")).await;
        assert_eq!(a.get("s1").await, Some(session("s1")));
    }

    #[tokio::test]
    async fn another_scope_cannot_see_the_row() {
        let store = Arc::new(InMemoryScopedSessionStore::new());
        repo(&store, "ws_a").save(session("s1")).await;
        // Same session id, different bound scope → invisible (the isolation fence).
        assert_eq!(repo(&store, "ws_b").get("s1").await, None);
    }

    #[tokio::test]
    async fn the_same_id_is_independent_per_scope() {
        let store = Arc::new(InMemoryScopedSessionStore::new());
        let mut a_row = session("s1");
        a_row.title = Some("acme".into());
        let mut b_row = session("s1");
        b_row.title = Some("beta".into());
        repo(&store, "ws_a").save(a_row.clone()).await;
        repo(&store, "ws_b").save(b_row.clone()).await;
        assert_eq!(repo(&store, "ws_a").get("s1").await, Some(a_row));
        assert_eq!(repo(&store, "ws_b").get("s1").await, Some(b_row));
    }

    #[tokio::test]
    async fn the_decorator_exposes_its_bound_scope() {
        let store = Arc::new(InMemoryScopedSessionStore::new());
        assert_eq!(repo(&store, "ws_a").scope(), &ScopeId::from("ws_a"));
    }
}
