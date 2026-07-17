//! In-memory reference session-repository backends.
//!
//! [`InMemorySessionRepository`] is the open-tier single-process default the Managed
//! state wires when no durable backend is configured; [`InMemoryScopedSessionStore`] is
//! the executable definition of the tenant-isolation fence used by tests and the
//! scoped default. Both live here beside the durable sqlite/postgres siblings; the
//! neutral ports + `PersistedSession` value + the `ScopedSessionRepo` decorator they
//! compose live inward in `awaken-session-contract`.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_session_contract::{ManagedSessionRepository, PersistedSession, ScopedSessionStore};
use awaken_tenancy::ScopeId;

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
    use std::collections::BTreeMap;

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
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use awaken_session_contract::ScopedSessionRepo;

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
