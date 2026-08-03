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
    ConfigRepositoryError, InferenceProfile, InferenceProfileStore, WebhookAuthoringPatch,
    WebhookAuthoringState, WebhookDeliveryOutcome, WebhookDeliveryState, WebhookEndpointDef,
    WebhookMutationIntent, WebhookStore, validate_agent_input_revision,
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

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
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
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
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
    fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending: bool = tx
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {NS}_webhook_mutation WHERE id = ?1)"),
                params![&patch.id],
                |row| row.get(0),
            )
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if pending {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {} has a pending material mutation",
                patch.id
            )));
        }
        let data: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook WHERE id = ?1"),
                params![&patch.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let Some(data) = data else {
            return Ok(WebhookAuthoringState::Missing);
        };
        let mut definition: WebhookEndpointDef = serde_json::from_str(&data)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if definition.workspace_id != patch.workspace_id {
            return Ok(WebhookAuthoringState::OwnerMismatch);
        }
        definition.url = patch.url;
        definition.event_types = patch.event_types;
        if let Some(disabled) = patch.disabled {
            definition.disabled = disabled;
            if !disabled {
                definition.consecutive_failures = 0;
            }
        }
        let data = serde_json::to_string(&definition)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.execute(
            &format!("UPDATE {NS}_webhook SET data = ?2 WHERE id = ?1"),
            params![&patch.id, data],
        )
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(WebhookAuthoringState::Updated(definition))
    }

    fn begin_mutation(&self, intent: WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook_mutation WHERE id = ?1"),
                params![&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if let Some(pending) = pending {
            let pending: WebhookMutationIntent = serde_json::from_str(&pending)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            return if pending == intent {
                Ok(())
            } else {
                Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} already has a pending mutation"
                )))
            };
        }
        let current: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook WHERE id = ?1"),
                params![&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let current = current
            .as_deref()
            .map(serde_json::from_str::<WebhookEndpointDef>)
            .transpose()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if current != intent.before {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} changed before mutation admission"
            )));
        }
        let data = serde_json::to_string(&intent)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.execute(
            &format!("INSERT INTO {NS}_webhook_mutation(id,data) VALUES (?1,?2)"),
            params![&id, data],
        )
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    }

    fn apply_mutation(&self, intent: &WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook_mutation WHERE id = ?1"),
                params![&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending = pending
            .as_deref()
            .map(serde_json::from_str::<WebhookMutationIntent>)
            .transpose()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let current: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook WHERE id = ?1"),
                params![&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let current = current
            .as_deref()
            .map(serde_json::from_str::<WebhookEndpointDef>)
            .transpose()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if pending.as_ref() != Some(intent) || current != intent.before {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} no longer matches its pending mutation"
            )));
        }
        match &intent.after {
            Some(after) => {
                let data = serde_json::to_string(after)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                tx.execute(
                    &format!("INSERT INTO {NS}_webhook(id,data) VALUES (?1,?2)"),
                    params![&id, data],
                )
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            }
            None => {
                tx.execute(
                    &format!("DELETE FROM {NS}_webhook WHERE id = ?1"),
                    params![&id],
                )
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            }
        }
        tx.commit()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    }

    fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let mut statement = conn
            .prepare(&format!(
                "SELECT data FROM {NS}_webhook_mutation ORDER BY id"
            ))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        rows.map(|row| {
            let data = row.map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            serde_json::from_str(&data)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .collect()
    }

    fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook_mutation WHERE id = ?1"),
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let Some(pending) = pending else {
            return Ok(());
        };
        let pending: WebhookMutationIntent = serde_json::from_str(&pending)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if &pending != intent {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} has a different pending mutation"
            )));
        }
        tx.execute(
            &format!("DELETE FROM {NS}_webhook_mutation WHERE id = ?1"),
            params![id],
        )
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(())
    }

    fn material_refs(
        &self,
    ) -> Result<Vec<awaken_credential_vault::SecretRef>, ConfigRepositoryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let mut statement = conn
            .prepare(&format!("SELECT data FROM {NS}_webhook ORDER BY id"))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        rows.map(|row| {
            let data = row.map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            serde_json::from_str::<WebhookEndpointDef>(&data)
                .map(|definition| definition.secret_ref)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .collect()
    }

    fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("admin store mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pending: bool = tx
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {NS}_webhook_mutation WHERE id = ?1)"),
                params![id],
                |row| row.get(0),
            )
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        if pending {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} has a pending material mutation"
            )));
        }
        let data: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_webhook WHERE id = ?1"),
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let Some(data) = data else {
            return Ok(WebhookDeliveryState::Missing);
        };
        let mut definition: WebhookEndpointDef = serde_json::from_str(&data)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let state = definition.record_delivery(outcome, failure_threshold);
        let data = serde_json::to_string(&definition)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.execute(
            &format!("UPDATE {NS}_webhook SET data = ?2 WHERE id = ?1"),
            params![id, data],
        )
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(state)
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
    fn webhook_failure_state_survives_reopen_and_disables_atomically() {
        // Cause/effect graph: C1 durable row; C2 failed delivery; C3 process/store
        // reopen; C4 second failure reaches threshold. Effects E1 count=1 on disk;
        // E2 reopen observes it; E3 row disabled at count=2. Decision table:
        // R1=C1∧C2 -> E1; R2=R1∧C3 -> E2; R3=R2∧C4 -> E3.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        let path = path.to_str().unwrap();
        {
            let store = SqliteAdminStore::open(path).unwrap();
            let intent = WebhookMutationIntent::create(WebhookEndpointDef {
                id: "wh".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/hook".into(),
                event_types: vec![],
                disabled: false,
                consecutive_failures: 0,
                secret_ref: awaken_credential_vault::SecretRef("whsec:wh".into()),
            });
            store.begin_mutation(intent.clone()).unwrap();
            store.apply_mutation(&intent).unwrap();
            store.complete_mutation(&intent).unwrap();
            assert_eq!(
                store
                    .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                    .unwrap(),
                WebhookDeliveryState::Active {
                    consecutive_failures: 1
                },
                "R1"
            );
        }
        let reopened = SqliteAdminStore::open(path).unwrap();
        assert_eq!(
            WebhookStore::get(&reopened, "wh")
                .unwrap()
                .unwrap()
                .consecutive_failures,
            1,
            "R2"
        );
        assert_eq!(
            reopened
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .unwrap(),
            WebhookDeliveryState::Disabled {
                consecutive_failures: 2
            },
            "R3"
        );
        assert!(
            WebhookStore::get(&reopened, "wh")
                .unwrap()
                .unwrap()
                .disabled,
            "R3"
        );
    }

    #[test]
    fn webhook_material_intent_survives_reopen_and_fences_other_writers() {
        // Cause/effect decision table: R1 begin create + reopen -> intent remains;
        // R2 pending intent + delivery/update -> mutation conflict; R3 apply +
        // complete -> row committed and journal empty; R4 later delivery + authored
        // update -> counter/ref preserved while URL changes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-intent.db");
        let path = path.to_str().unwrap();
        let definition = WebhookEndpointDef {
            id: "wh".into(),
            workspace_id: "ws".into(),
            url: "https://hooks.example/old".into(),
            event_types: vec![],
            disabled: false,
            consecutive_failures: 0,
            secret_ref: awaken_credential_vault::SecretRef("sec:webhook:wh".into()),
        };
        let intent = WebhookMutationIntent::create(definition.clone());
        {
            let store = SqliteAdminStore::open(path).unwrap();
            store.begin_mutation(intent.clone()).unwrap();
        }
        let store = SqliteAdminStore::open(path).unwrap();
        assert_eq!(
            store.pending_mutations().unwrap(),
            vec![intent.clone()],
            "R1"
        );
        assert!(
            matches!(
                store.record_delivery("wh", WebhookDeliveryOutcome::Failed, 2),
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2"
        );
        assert!(
            matches!(
                store.update_authored(WebhookAuthoringPatch {
                    id: "wh".into(),
                    workspace_id: "ws".into(),
                    url: "https://hooks.example/racing".into(),
                    event_types: vec![],
                    disabled: None,
                }),
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2"
        );
        store.apply_mutation(&intent).unwrap();
        store.complete_mutation(&intent).unwrap();
        assert!(store.pending_mutations().unwrap().is_empty(), "R3");
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
            .unwrap();
        let updated = store
            .update_authored(WebhookAuthoringPatch {
                id: "wh".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/new".into(),
                event_types: vec!["run.completed".into()],
                disabled: None,
            })
            .unwrap();
        let WebhookAuthoringState::Updated(updated) = updated else {
            panic!("R4 must update")
        };
        assert_eq!(updated.url, "https://hooks.example/new", "R4");
        assert_eq!(updated.consecutive_failures, 1, "R4");
        assert_eq!(updated.secret_ref, definition.secret_ref, "R4");
    }

    #[test]
    fn concurrent_sqlite_authoring_and_delivery_do_not_lose_operational_state() {
        // Cause/effect graph: C1 two independent SQLite connections; C2 authored
        // URL update; C3 failed-delivery increment; C2 and C3 start together.
        // Effects E1 both operations commit in some order, E2 final URL is new,
        // E3 final counter is one, E4 secret ref is unchanged. The IMMEDIATE
        // transaction is the absent/independent-connection serialization edge.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-race.db");
        let path = path.to_str().unwrap().to_string();
        let seed_store = SqliteAdminStore::open(&path).unwrap();
        let definition = WebhookEndpointDef {
            id: "wh".into(),
            workspace_id: "ws".into(),
            url: "https://hooks.example/old".into(),
            event_types: vec![],
            disabled: false,
            consecutive_failures: 0,
            secret_ref: awaken_credential_vault::SecretRef("sec:webhook:race".into()),
        };
        let intent = WebhookMutationIntent::create(definition.clone());
        seed_store.begin_mutation(intent.clone()).unwrap();
        seed_store.apply_mutation(&intent).unwrap();
        seed_store.complete_mutation(&intent).unwrap();
        drop(seed_store);

        let authoring = SqliteAdminStore::open(&path).unwrap();
        let delivery = SqliteAdminStore::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let authoring_barrier = barrier.clone();
        let authoring_thread = std::thread::spawn(move || {
            authoring_barrier.wait();
            authoring.update_authored(WebhookAuthoringPatch {
                id: "wh".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/new".into(),
                event_types: vec!["run.completed".into()],
                disabled: None,
            })
        });
        let delivery_barrier = barrier.clone();
        let delivery_thread = std::thread::spawn(move || {
            delivery_barrier.wait();
            delivery.record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
        });
        barrier.wait();
        assert!(authoring_thread.join().unwrap().is_ok(), "E1 authoring");
        assert!(delivery_thread.join().unwrap().is_ok(), "E1 delivery");

        let final_store = SqliteAdminStore::open(&path).unwrap();
        let final_row = WebhookStore::get(&final_store, "wh").unwrap().unwrap();
        assert_eq!(final_row.url, "https://hooks.example/new", "E2");
        assert_eq!(final_row.consecutive_failures, 1, "E3");
        assert_eq!(final_row.secret_ref, definition.secret_ref, "E4");
    }

    #[test]
    fn published_v1_v2_ledger_upgrades_to_v11() {
        // Cause/effect decision table:
        // | starting ledger | canonical bundle | effect                         |
        // | empty           | V1..V11          | full schema; intent WAL present |
        // | V1,V2           | V1..V11          | V3..V11; profile survives       |
        // | V1,V2           | rewritten V1     | fail closed on unknown V2      |
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = admin_bundle().expect("bundle builds");
        let published_v1_v2 = awaken_scoped_migration::MigrationBundle::new(
            crate::schema::BUNDLE_ID,
            full.migrations()[..2].to_vec(),
        )
        .expect("published V1/V2 bundle");
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner
            .run_bundle(&conn, &published_v1_v2)
            .expect("apply V1/V2");
        conn.execute(
            "INSERT INTO admin_inference_profile(id,data) VALUES ('kept','{}')",
            [],
        )
        .expect("seed profile");

        let delta = runner.run_bundle(&conn, &full).expect("upgrade to V11");
        assert_eq!(
            delta
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            (3..=11).collect::<Vec<_>>()
        );
        let kept: String = conn
            .query_row(
                "SELECT data FROM admin_inference_profile WHERE id='kept'",
                [],
                |row| row.get(0),
            )
            .expect("read pre-upgrade profile");
        assert_eq!(kept, "{}");
    }

    #[test]
    fn published_v9_ledger_retires_the_legacy_webhook_outbox() {
        // Cause/effect graph: C1 V1..V9 receipts + legacy outbox present; C2 V10
        // receipt absent; C3 canonical V1..V10 bundle. Effect E1 applies only V10,
        // E2 removes the competing outbox table, E3 records V10. Decision rule
        // R1=C1∧C2∧C3 -> E1∧E2∧E3; V11 then installs the one material-intent
        // journal, and replay applies no migration.
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = admin_bundle().expect("bundle builds");
        let published_v9 = awaken_scoped_migration::MigrationBundle::new(
            crate::schema::BUNDLE_ID,
            full.migrations()[..9].to_vec(),
        )
        .expect("published V1..V9 bundle");
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner
            .run_bundle(&conn, &published_v9)
            .expect("apply V1..V9");
        conn.execute(
            "INSERT INTO admin_webhook_outbox(event_id,data) VALUES ('legacy','{}')",
            [],
        )
        .expect("seed legacy outbox");

        let delta = runner.run_bundle(&conn, &full).expect("apply V10");
        assert_eq!(
            delta
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            vec![10, 11],
            "R1/E1/E3"
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'admin_webhook_outbox'",
                [],
                |row| row.get(0),
            )
            .expect("inspect schema");
        assert_eq!(count, 0, "R1/E2");
        assert!(
            runner
                .run_bundle(&conn, &full)
                .expect("replay V10/V11")
                .is_empty(),
            "the scoped receipt makes R1 idempotent"
        );
    }

    #[test]
    fn control_admin_schema_exposes_only_owned_active_adapters() {
        // Cause/effect decision table:
        // R1 active Control profile/input/webhook aggregates and webhook mutation
        // journal -> present.
        // R2 retired MCP tracks -> dropped by their published retirement migration.
        // R3 historical memory/catalog DDL has no current repository adapter.
        // R4 the historical admin webhook outbox is dropped because the Session
        // repository lifecycle outbox is the sole delivery authority.
        let store = SqliteAdminStore::open_in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        for table in [
            "admin_inference_profile",
            "admin_agent_resource",
            "admin_webhook",
            "admin_webhook_mutation",
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
            "admin_resource_catalog_entry",
            "admin_webhook_outbox",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "R2/R3/R4: {table}");
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
