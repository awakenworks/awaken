//! SQLite adapter (feature `sqlite`, ADR-0043 sqlite-repos) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`SqliteAdminStore`] serves **both** sync store ports —
//! [`InferenceProfileStore`] and [`McpStore`] — from a single database
//! connection (one `admin.db` file; the two aggregates share one bundle, so one
//! connection keeps the composition root simple and the ledger in one place).
//!
//! The store ports are *sync* and have no error channel (their contract, like
//! the in-memory impls' `lock().expect(..)`, is that a broken store is a
//! panic-worthy invariant violation, not a recoverable condition), so unlike
//! the async catalog/credential adapters there is no `spawn_blocking` — every
//! call is one short statement under the connection mutex.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use awaken_config_resolver::{AgentMcpConfig, InferenceProfile, McpServerDef};

use crate::router::{InferenceProfileStore, McpStore};
use crate::schema::admin_bundle;

/// The admin component's table namespace (its bundle prefix).
const NS: &str = "admin";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed store for the admin-plane aggregates: one database file
/// (or in-memory connection) implementing both [`InferenceProfileStore`] and
/// [`McpStore`]. Clone the `Arc<SqliteAdminStore>` into both `AdminState`
/// slots so the two ports share the row set.
pub struct SqliteAdminStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteAdminStore {
    /// Open (or create) a database file and apply the admin migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::over(conn)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::over(conn)
    }

    fn over(conn: Connection) -> Result<Self, StoreError> {
        let bundle = admin_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Upsert one JSON row by primary key.
    fn put_row<T: serde::Serialize>(&self, table: &str, key_col: &str, key: &str, value: &T) {
        let data = serde_json::to_string(value).expect("serialize admin row");
        self.conn
            .lock()
            .expect("admin store")
            .execute(
                &format!(
                    "INSERT INTO {NS}_{table} ({key_col}, data) VALUES (?1, ?2) \
                     ON CONFLICT({key_col}) DO UPDATE SET data = excluded.data"
                ),
                params![key, data],
            )
            .expect("write admin row");
    }

    /// One JSON row by primary key, deserialized; `None` when absent.
    fn get_row<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
    ) -> Option<T> {
        let data: Option<String> = self
            .conn
            .lock()
            .expect("admin store")
            .query_row(
                &format!("SELECT data FROM {NS}_{table} WHERE {key_col} = ?1"),
                params![key],
                |row| row.get(0),
            )
            .optional()
            .expect("read admin row");
        data.map(|d| serde_json::from_str(&d).expect("decode admin row"))
    }
}

impl InferenceProfileStore for SqliteAdminStore {
    fn put(&self, id: String, profile: InferenceProfile) {
        self.put_row("inference_profile", "id", &id, &profile);
    }
    fn get(&self, id: &str) -> Option<InferenceProfile> {
        self.get_row("inference_profile", "id", id)
    }
}

impl McpStore for SqliteAdminStore {
    fn put_server(&self, def: McpServerDef) {
        self.put_row("mcp_server", "id", &def.id.0.clone(), &def);
    }
    fn get_server(&self, id: &str) -> Option<McpServerDef> {
        self.get_row("mcp_server", "id", id)
    }
    fn list_servers(&self) -> Vec<McpServerDef> {
        // Sorted by id, matching the in-memory store's ordering contract.
        let conn = self.conn.lock().expect("admin store");
        let mut stmt = conn
            .prepare(&format!("SELECT data FROM {NS}_mcp_server ORDER BY id"))
            .expect("prepare admin list");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("list admin rows");
        rows.map(|data| {
            serde_json::from_str(&data.expect("read admin row")).expect("decode admin row")
        })
        .collect()
    }
    fn put_agent_config(&self, config: AgentMcpConfig) {
        self.put_row("agent_mcp", "agent_id", &config.agent_id.clone(), &config);
    }
    fn get_agent_config(&self, agent_id: &str) -> Option<AgentMcpConfig> {
        self.get_row("agent_mcp", "agent_id", agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::McpServerId;
    use awaken_credential_vault::CredentialBinding;

    fn profile(model: &str) -> InferenceProfile {
        InferenceProfile {
            model_id: model.to_string(),
            credential_binding: CredentialBinding::None,
            disabled_endpoint_ids: vec![],
        }
    }

    fn server(id: &str) -> McpServerDef {
        McpServerDef {
            id: McpServerId(id.to_string()),
            display_name: id.to_string(),
            url: format!("http://{id}.example/"),
            credential_binding: CredentialBinding::None,
            version: 1,
        }
    }

    #[test]
    fn profile_round_trip_and_overwrite() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        assert!(InferenceProfileStore::get(&store, "p1").is_none());
        store.put("p1".into(), profile("m1"));
        assert_eq!(
            InferenceProfileStore::get(&store, "p1").unwrap().model_id,
            "m1"
        );
        store.put("p1".into(), profile("m2"));
        assert_eq!(
            InferenceProfileStore::get(&store, "p1").unwrap().model_id,
            "m2"
        );
    }

    #[test]
    fn mcp_server_and_agent_config_round_trip_sorted() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        store.put_server(server("zeta"));
        store.put_server(server("alpha"));
        assert_eq!(
            store.get_server("zeta").unwrap().url,
            "http://zeta.example/"
        );
        assert!(store.get_server("missing").is_none());
        let ids: Vec<String> = store.list_servers().into_iter().map(|s| s.id.0).collect();
        assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);

        let config = AgentMcpConfig {
            agent_id: "agent-1".into(),
            mcp_server_ids: vec![McpServerId("alpha".into())],
            version: 1,
        };
        store.put_agent_config(config.clone());
        assert_eq!(
            store.get_agent_config("agent-1").unwrap().mcp_server_ids,
            config.mcp_server_ids
        );
        assert!(store.get_agent_config("agent-2").is_none());
    }

    #[test]
    fn rows_survive_a_reopen_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        let path = path.to_str().unwrap();
        {
            let store = SqliteAdminStore::open(path).unwrap();
            store.put("p1".into(), profile("m1"));
            store.put_server(server("calc"));
            store.put_agent_config(AgentMcpConfig {
                agent_id: "agent-1".into(),
                mcp_server_ids: vec![McpServerId("calc".into())],
                version: 1,
            });
        }
        // Reopen: the migration is idempotent and the rows are still there.
        let store = SqliteAdminStore::open(path).unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1").unwrap().model_id,
            "m1"
        );
        assert_eq!(store.list_servers().len(), 1);
        assert_eq!(
            store.get_agent_config("agent-1").unwrap().mcp_server_ids,
            vec![McpServerId("calc".into())]
        );
    }
}
