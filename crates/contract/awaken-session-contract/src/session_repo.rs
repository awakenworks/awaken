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

    /// Record the owner scope of `session_id` (ADR-0051): the opaque `scope_id`
    /// that created it, so the edge ownership guard can fence a cross-tenant
    /// request even after a restart lost the in-memory index. Durable backends
    /// persist it beside the (tenancy-agnostic) config row; the in-memory default
    /// no-ops, since same-process ownership lives in `ManagedState`'s index.
    async fn set_owner(&self, _session_id: &str, _scope: &str) {}

    /// The persisted owner scope of `session_id`, if this backend records one
    /// (durable backends only; the in-memory default returns `None`).
    async fn owner(&self, _session_id: &str) -> Option<String> {
        None
    }
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
mod tests {
    use super::*;
    use serde_json::json;

    fn session(id: &str) -> PersistedSession {
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "assistant".into(),
            model: "kimi".into(),
            title: Some("Demo".into()),
            metadata: BTreeMap::from([("k".to_string(), "v".to_string())]),
            environment_id: "env".into(),
            // The wire-echo shape the agent object reports — never a credential.
            mcp_servers: vec![json!({"name": "gh", "type": "url", "url": "https://mcp.example"})],
        }
    }

    // Item 3: the non-scoped in-memory repo round-trips a save→get, and an upsert by
    // `session_id` replaces the prior row (idempotent by id).
    #[tokio::test]
    async fn in_memory_repo_saves_and_reads_back() {
        let repo = InMemorySessionRepository::default();
        assert_eq!(repo.get("s1").await, None, "empty before any save");
        repo.save(session("s1")).await;
        assert_eq!(repo.get("s1").await, Some(session("s1")));

        // Upsert by id: a second save under the same id replaces the row.
        let mut updated = session("s1");
        updated.title = Some("Renamed".into());
        repo.save(updated.clone()).await;
        assert_eq!(repo.get("s1").await, Some(updated));
    }

    // Item 3: the tenancy-owner hooks default to no-op / None on the in-memory repo
    // (same-process ownership lives in `ManagedState`'s index, not this store).
    #[tokio::test]
    async fn in_memory_repo_owner_defaults_are_noop_and_none() {
        let repo = InMemorySessionRepository::default();
        repo.save(session("s1")).await;
        // `set_owner` is a no-op that must not error or affect `get`.
        repo.set_owner("s1", "ws_a").await;
        assert_eq!(repo.get("s1").await, Some(session("s1")));
        // `owner` reports nothing for a backend that records no scope.
        assert_eq!(repo.owner("s1").await, None);
    }

    // Item 6: `PersistedSession` equality + a serde-free structural round-trip through
    // the port, carrying `mcp_servers: Vec<Value>` faithfully (the wire echo survives).
    #[tokio::test]
    async fn persisted_session_carries_mcp_servers_and_compares_by_value() {
        let a = session("s1");
        let b = session("s1");
        assert_eq!(a, b, "same fields ⇒ equal");

        let mut differ = session("s1");
        differ.mcp_servers = vec![json!({"name": "other"})];
        assert_ne!(a, differ, "differing mcp_servers ⇒ not equal");

        // The Vec<Value> survives a store round-trip intact.
        let repo = InMemorySessionRepository::default();
        repo.save(a.clone()).await;
        let back = repo.get("s1").await.expect("row exists");
        assert_eq!(back.mcp_servers, a.mcp_servers);
        assert_eq!(
            back.mcp_servers[0]["url"],
            json!("https://mcp.example"),
            "the wire-echo url is preserved"
        );
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
