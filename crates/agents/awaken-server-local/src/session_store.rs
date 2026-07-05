//! A SQLite-backed [`ManagedSessionRepository`]: the durable home for the Managed
//! session aggregate (agent / model / title / metadata / accepted MCP servers).
//!
//! It is its OWN database file (`sessions.db`), NOT a table in the authoring-plane
//! `admin.db`: a live session instance is a different aggregate from the agent/MCP
//! *definitions* that admin.db holds, so mixing them would cross a bounded-context
//! line (ADR-0039 "one repository per aggregate"). Secrets never land here — only
//! the wire-echo MCP `{name,type,url}` values, per the port's contract (G3).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_protocol_managed::{ManagedSessionRepository, PersistedSession};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// SQLite persistence for [`PersistedSession`]. One row per session, keyed by id.
pub struct SqliteManagedSessionRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteManagedSessionRepository {
    /// Open (or create) `sessions.db` at `path` and apply the schema.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    /// An in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS managed_session (
                session_id     TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                model          TEXT NOT NULL,
                title          TEXT,
                metadata_json  TEXT NOT NULL,
                environment_id TEXT NOT NULL,
                mcp_json       TEXT NOT NULL
            )",
            [],
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl ManagedSessionRepository for SqliteManagedSessionRepository {
    async fn save(&self, session: PersistedSession) {
        let metadata_json =
            serde_json::to_string(&session.metadata).expect("session metadata serializes");
        let mcp_json =
            serde_json::to_string(&session.mcp_servers).expect("session mcp servers serialize");
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.execute(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                model = excluded.model,
                title = excluded.title,
                metadata_json = excluded.metadata_json,
                environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json",
            params![
                session.session_id,
                session.agent_id,
                session.model,
                session.title,
                metadata_json,
                session.environment_id,
                mcp_json,
            ],
        )
        .expect("persist managed session");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.query_row(
            "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json
             FROM managed_session WHERE session_id = ?1",
            params![session_id],
            |row| {
                let metadata_json: String = row.get(3)?;
                let mcp_json: String = row.get(5)?;
                let metadata: BTreeMap<String, String> =
                    serde_json::from_str(&metadata_json).unwrap_or_default();
                let mcp_servers: Vec<Value> = serde_json::from_str(&mcp_json).unwrap_or_default();
                Ok(PersistedSession {
                    session_id: session_id.to_string(),
                    agent_id: row.get(0)?,
                    model: row.get(1)?,
                    title: row.get(2)?,
                    metadata,
                    environment_id: row.get(4)?,
                    mcp_servers,
                })
            },
        )
        .optional()
        .expect("read managed session")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "coder".to_string(),
            model: "kimi-k2".to_string(),
            title: Some("My session".to_string()),
            metadata,
            environment_id: "env_local".to_string(),
            mcp_servers: vec![serde_json::json!({"name":"calc","type":"url","url":"https://x"})],
        }
    }

    #[tokio::test]
    async fn round_trips_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let path = path.to_string_lossy().to_string();

        // First process: create + persist, then drop the repo (simulated exit).
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            repo.save(sample("sesn_1")).await;
            assert_eq!(repo.get("sesn_1").await, Some(sample("sesn_1")));
        }
        // Second process: a fresh repo over the same file restores the row.
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(
            reopened.get("sesn_1").await,
            Some(sample("sesn_1")),
            "the session config survives a restart"
        );
        assert!(reopened.get("sesn_missing").await.is_none());
    }

    #[tokio::test]
    async fn save_is_an_idempotent_upsert() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        repo.save(sample("sesn_1")).await;
        let mut updated = sample("sesn_1");
        updated.title = Some("Renamed".to_string());
        repo.save(updated.clone()).await;
        assert_eq!(
            repo.get("sesn_1").await,
            Some(updated),
            "re-save overwrites"
        );
    }
}
