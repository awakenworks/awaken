//! SQLite adapter (feature `sqlite`, ADR-0043 sqlite-repos) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`SqliteAdminStore`] serves the admin aggregate ports from a single
//! database connection and migration ledger.
//!
//! Repository ports expose storage errors to the application layer; malformed
//! rows and unavailable SQLite storage therefore become HTTP 500 responses rather
//! than panics or false not-found results. Calls remain synchronous and short under
//! the connection mutex.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, AgentInputRepositoryError,
    ConfigRepositoryError, InferenceProfile, InferenceProfileStore, WebhookEndpointDef,
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

/// A SQLite-backed store for the admin-plane aggregates using one database file
/// (or in-memory connection) and one scoped migration ledger.
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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Upsert one JSON row by primary key.
    fn put_row<T: serde::Serialize>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
        value: &T,
    ) -> Result<(), ConfigRepositoryError> {
        let data = serde_json::to_string(value)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        self.conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?
            .execute(
                &format!(
                    "INSERT INTO {NS}_{table} ({key_col}, data) VALUES (?1, ?2) \
                     ON CONFLICT({key_col}) DO UPDATE SET data = excluded.data"
                ),
                params![key, data],
            )
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(())
    }

    /// One JSON row by primary key, deserialized; `None` when absent.
    fn get_row<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
    ) -> Result<Option<T>, ConfigRepositoryError> {
        let data: Option<String> = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?
            .query_row(
                &format!("SELECT data FROM {NS}_{table} WHERE {key_col} = ?1"),
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        data.map(|data| {
            serde_json::from_str(&data)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .transpose()
    }
}

impl AgentInputBindingRepository for SqliteAdminStore {
    fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = format!("{workspace_id}\u{1f}{}", config.agent_id);
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| AgentInputRepositoryError::Storage("admin store mutex poisoned".into()))?;
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
    fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError> {
        let key = format!("{workspace_id}\u{1f}{agent_id}");
        self.get_row("agent_resource", "agent_id", &key)
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
    }
    fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError> {
        let prefix = format!("{workspace_id}\u{1f}");
        let conn = self
            .conn
            .lock()
            .map_err(|_| AgentInputRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let mut statement = conn
            .prepare(&format!(
                "SELECT agent_id, data FROM {NS}_agent_resource ORDER BY agent_id"
            ))
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        rows.map(|row| row.map_err(|error| AgentInputRepositoryError::Storage(error.to_string())))
            .filter(|row| {
                row.as_ref()
                    .map_or(true, |(key, _)| key.starts_with(&prefix))
            })
            .map(|row| {
                let (_, data) = row?;
                serde_json::from_str(&data)
                    .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
            })
            .collect()
    }
}

impl InferenceProfileStore for SqliteAdminStore {
    fn put(&self, id: String, profile: InferenceProfile) -> Result<(), ConfigRepositoryError> {
        self.put_row("inference_profile", "id", &id, &profile)
    }
    fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
        self.get_row("inference_profile", "id", id)
    }
}

impl WebhookStore for SqliteAdminStore {
    fn put(&self, def: WebhookEndpointDef) -> Result<(), ConfigRepositoryError> {
        self.put_row("webhook", "id", &def.id.clone(), &def)
    }
    fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError> {
        self.get_row("webhook", "id", id)
    }
    fn list(&self, workspace_id: &str) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError> {
        // Scan the (low-cardinality) webhook rows and filter by owner in Rust —
        // the generic row helpers key by id; workspace lives inside the JSON.
        let conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let mut stmt = conn
            .prepare(&format!("SELECT data FROM {NS}_webhook ORDER BY id"))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        rows.map(|data| {
            let data = data.map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            serde_json::from_str::<WebhookEndpointDef>(&data)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .filter(|row| {
            row.as_ref()
                .map_or(true, |def| def.workspace_id == workspace_id)
        })
        .collect()
    }
    fn delete(&self, id: &str) -> Result<bool, ConfigRepositoryError> {
        let n = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?
            .execute(
                &format!("DELETE FROM {NS}_webhook WHERE id = ?1"),
                params![id],
            )
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::{
        BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, ModelTarget,
        ProfileCandidate, ResourceAccess,
    };
    use awaken_credential_vault::CredentialBinding;

    fn profile(model: &str) -> InferenceProfile {
        InferenceProfile {
            workspace_id: "ws".into(),
            primary: ProfileCandidate {
                target: ModelTarget::unqualified(model),
                credential_binding: CredentialBinding::None,
            },
            fallbacks: Vec::new(),
            disabled_endpoint_ids: vec![],
        }
    }

    #[test]
    fn profile_round_trip_and_overwrite() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        assert!(InferenceProfileStore::get(&store, "p1").unwrap().is_none());
        InferenceProfileStore::put(&store, "p1".into(), profile("m1")).unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m1"
        );
        InferenceProfileStore::put(&store, "p1".into(), profile("m2")).unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m2"
        );
    }

    #[test]
    fn control_admin_schema_contains_only_owned_active_tables() {
        // Cause/effect decision table:
        // R1 active Control profile/input/webhook aggregates -> present.
        // R2 retired MCP/memory/outbox tracks -> absent.
        // R3 Resources-owned catalog -> absent from the Control database.
        let store = SqliteAdminStore::open_in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        for table in [
            "admin_inference_profile",
            "admin_agent_resource",
            "admin_webhook",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "R1: {table}");
        }
        for table in [
            "admin_mcp_server",
            "admin_agent_mcp",
            "admin_memory_store",
            "admin_webhook_outbox",
            "admin_resource_catalog_entry",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "R2/R3: {table}");
        }
    }

    #[test]
    fn agent_resource_binding_round_trips_and_overwrites() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        assert!(
            store
                .get_agent_inputs("workspace-a", "agent-1")
                .unwrap()
                .is_none()
        );

        let config = AgentInputConfig {
            agent_id: "agent-1".into(),
            environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                environment_id: "env-1".into(),
                revision: 3,
            }),
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
            Some(config.clone())
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
        let got = store
            .get_agent_inputs("workspace-a", "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(got.inputs.len(), 2);
        assert_eq!(got.environment.as_ref().unwrap().revision, 3);
        assert_eq!(got.revision, 2);
        assert!(
            store
                .get_agent_inputs("workspace-a", "agent-2")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_agent_inputs("workspace-b", "agent-1")
                .unwrap()
                .is_none(),
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
            InferenceProfileStore::put(&store, "p1".into(), profile("m1")).unwrap();
        }
        // Reopen: the migration is idempotent and the rows are still there.
        let store = SqliteAdminStore::open(path).unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m1"
        );
    }

    #[test]
    fn malformed_profile_row_uses_the_repository_error_channel() {
        let store = SqliteAdminStore::open_in_memory().unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                &format!("INSERT INTO {NS}_inference_profile(id, data) VALUES ('bad', '{{')"),
                [],
            )
            .unwrap();
        assert!(matches!(
            InferenceProfileStore::get(&store, "bad"),
            Err(ConfigRepositoryError::Storage(_))
        ));
    }
}
