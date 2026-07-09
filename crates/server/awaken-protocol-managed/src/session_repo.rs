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
use std::sync::Mutex;

use async_trait::async_trait;
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
