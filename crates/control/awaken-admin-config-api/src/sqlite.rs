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

use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, AgentInputRepositoryError, AgentMcpConfig,
    InferenceProfile, InferenceProfileStore, McpServerDef, McpStore, WebhookEndpointDef,
    WebhookStore, validate_agent_input_revision,
};

use crate::schema::admin_bundle;

/// The admin component's table namespace (its bundle prefix).
pub(crate) const NS: &str = "admin";

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
    pub(crate) conn: Arc<Mutex<Connection>>,
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
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store
            .migrate_legacy_memory_stores()
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        Ok(store)
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

impl AgentInputBindingRepository for SqliteAdminStore {
    fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = format!("{workspace_id}\u{1f}{}", config.agent_id);
        let mut conn = self.conn.lock().expect("admin store");
        let tx = conn
            .transaction()
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        let current: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_agent_resource WHERE agent_id = ?1"),
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        let current = current
            .as_deref()
            .map(serde_json::from_str::<AgentInputConfig>)
            .transpose()
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        if !validate_agent_input_revision(current.as_ref(), &config)? {
            return Ok(());
        }
        let data = serde_json::to_string(&config)
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        tx.execute(
            &format!(
                "INSERT INTO {NS}_agent_resource (agent_id, data) VALUES (?1, ?2) \
                 ON CONFLICT(agent_id) DO UPDATE SET data = excluded.data"
            ),
            params![key, data],
        )
        .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
    }
    fn get_agent_inputs(&self, workspace_id: &str, agent_id: &str) -> Option<AgentInputConfig> {
        let key = format!("{workspace_id}\u{1f}{agent_id}");
        self.get_row("agent_resource", "agent_id", &key)
    }
    fn list_agent_inputs(&self, workspace_id: &str) -> Vec<AgentInputConfig> {
        let prefix = format!("{workspace_id}\u{1f}");
        let conn = self.conn.lock().expect("admin store");
        let mut statement = conn
            .prepare(&format!(
                "SELECT agent_id, data FROM {NS}_agent_resource ORDER BY agent_id"
            ))
            .expect("list Agent input configs");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query Agent input configs")
            .filter_map(Result::ok)
            .filter(|(key, _)| key.starts_with(&prefix))
            .filter_map(|(_, data)| serde_json::from_str(&data).ok())
            .collect()
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

impl WebhookStore for SqliteAdminStore {
    fn put(&self, def: WebhookEndpointDef) {
        self.put_row("webhook", "id", &def.id.clone(), &def);
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.get_row("webhook", "id", id)
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        // Scan the (low-cardinality) webhook rows and filter by owner in Rust —
        // the generic row helpers key by id; workspace lives inside the JSON.
        let conn = self.conn.lock().expect("admin store");
        let mut stmt = conn
            .prepare(&format!("SELECT data FROM {NS}_webhook ORDER BY id"))
            .expect("prepare webhook list");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("list webhook rows");
        rows.map(|data| {
            serde_json::from_str::<WebhookEndpointDef>(&data.expect("read admin row"))
                .expect("decode admin row")
        })
        .filter(|d| d.workspace_id == workspace_id)
        .collect()
    }
    fn delete(&self, id: &str) -> bool {
        let n = self
            .conn
            .lock()
            .expect("admin store")
            .execute(
                &format!("DELETE FROM {NS}_webhook WHERE id = ?1"),
                params![id],
            )
            .expect("delete webhook row");
        n > 0
    }
    fn enqueue_outbox(&self, event: awaken_config_resolver::WebhookOutboxEvent) -> bool {
        let data = serde_json::to_string(&event).expect("encode webhook outbox event");
        self.conn
            .lock()
            .expect("admin store")
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO {NS}_webhook_outbox (event_id, data) VALUES (?1, ?2)"
                ),
                params![event.id, data],
            )
            .expect("enqueue webhook outbox event")
            > 0
    }
    fn pending_outbox(&self) -> Vec<awaken_config_resolver::WebhookOutboxEvent> {
        let conn = self.conn.lock().expect("admin store");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT data FROM {NS}_webhook_outbox ORDER BY created_at, event_id"
            ))
            .expect("prepare webhook outbox list");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("list webhook outbox");
        rows.map(|data| {
            serde_json::from_str(&data.expect("read webhook outbox row"))
                .expect("decode webhook outbox row")
        })
        .collect()
    }
    fn complete_outbox(&self, event_id: &str) -> bool {
        self.conn
            .lock()
            .expect("admin store")
            .execute(
                &format!("DELETE FROM {NS}_webhook_outbox WHERE event_id = ?1"),
                params![event_id],
            )
            .expect("complete webhook outbox event")
            > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::{
        BindingId, FileId, InputBinding, InputResourceId, McpServerId, MemoryStoreId,
        ResourceAccess,
    };
    use awaken_credential_vault::CredentialBinding;

    fn profile(model: &str) -> InferenceProfile {
        InferenceProfile {
            workspace_id: "ws".into(),
            model_id: model.to_string(),
            model_fallbacks: Vec::new(),
            credential_binding: CredentialBinding::None,
            disabled_endpoint_ids: vec![],
        }
    }

    fn server(id: &str) -> McpServerDef {
        McpServerDef {
            workspace_id: "ws".into(),
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
        InferenceProfileStore::put(&store, "p1".into(), profile("m1"));
        assert_eq!(
            InferenceProfileStore::get(&store, "p1").unwrap().model_id,
            "m1"
        );
        InferenceProfileStore::put(&store, "p1".into(), profile("m2"));
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
            workspace_id: "ws".into(),
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
    fn agent_resource_binding_round_trips_and_overwrites() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        assert!(store.get_agent_inputs("workspace-a", "agent-1").is_none());

        let config = AgentInputConfig {
            agent_id: "agent-1".into(),
            inputs: vec![InputBinding {
                binding_id: BindingId::from("memory"),
                target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
                mount_path: "/mnt/memory/prefs".into(),
                access: ResourceAccess::ReadWrite,
                instructions: Some("user preferences".into()),
            }],
            revision: 1,
        };
        store
            .put_agent_inputs("workspace-a", config.clone())
            .unwrap();
        assert_eq!(
            store.get_agent_inputs("workspace-a", "agent-1").unwrap(),
            config
        );

        // Upsert by agent_id replaces the whole binding set.
        let mut v2 = config.clone();
        v2.inputs.push(InputBinding {
            binding_id: BindingId::from("file"),
            target: InputResourceId::File(FileId::from("file-1")),
            mount_path: "/mnt/files/input.txt".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        });
        v2.revision = 2;
        store.put_agent_inputs("workspace-a", v2).unwrap();
        let got = store.get_agent_inputs("workspace-a", "agent-1").unwrap();
        assert_eq!(got.inputs.len(), 2);
        assert_eq!(got.revision, 2);
        assert!(store.get_agent_inputs("workspace-a", "agent-2").is_none());
        assert!(
            store.get_agent_inputs("workspace-b", "agent-1").is_none(),
            "Workspace is a mandatory aggregate key"
        );
    }

    #[test]
    fn rows_survive_a_reopen_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        let path = path.to_str().unwrap();
        {
            let store = SqliteAdminStore::open(path).unwrap();
            InferenceProfileStore::put(&store, "p1".into(), profile("m1"));
            store.put_server(server("calc"));
            store.put_agent_config(AgentMcpConfig {
                workspace_id: "ws".into(),
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
