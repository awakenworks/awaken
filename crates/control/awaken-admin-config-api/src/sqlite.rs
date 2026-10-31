// @formal entrypoint agent_inputs.rejected_revision_does_not_mutate
//! SQLite adapter (feature `sqlite`, ADR-0043 sqlite-repos) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`SqliteAdminStore`] serves the admin aggregate ports from a single
//! one-connection sqlx pool and migration ledger (ADR-0087).
//!
//! Repository ports expose storage errors to the application layer; malformed
//! rows and unavailable SQLite storage therefore become HTTP 500 responses rather
//! than panics or false not-found results. The pool is opened under the one
//! process-wide embedded SQLite policy.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]
use awaken_sqlite_pool::EmbeddedPool;

use awaken_config_resolver::{
    AgentInputAdmission, AgentInputBindingRepository, AgentInputConfig, AgentInputRepositoryError,
    ConfigRepositoryError, InferenceProfile, InferenceProfileStore, WebhookAuthoringPatch,
    WebhookAuthoringState, WebhookDeliveryOutcome, WebhookDeliveryState, WebhookEndpointDef,
    WebhookMutationIntent, WebhookStore, agent_input_storage_key, agent_input_workspace_prefix,
    plan_agent_input_write, validate_stored_agent_inputs,
};
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedSqlite;

use crate::schema::{BUNDLE_ID, converged_admin_bundle, selected_admin_bundle};

mod mcp;
mod telemetry;

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
/// (or in-memory database) and one scoped migration ledger.
pub struct SqliteAdminStore {
    pub(crate) pool: EmbeddedPool,
}

impl SqliteAdminStore {
    /// Open (or create) a database file and apply the admin migrations.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::file(path)).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::memory()).await
    }

    async fn open_at(location: EmbeddedSqlite) -> Result<Self, StoreError> {
        let pool = location
            .open()
            .await
            .map_err(|error| StoreError::Open(error.to_string()))?;
        Self::over(pool).await
    }

    async fn over(pool: EmbeddedPool) -> Result<Self, StoreError> {
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Upsert one JSON row by primary key.
    async fn put_row<T: serde::Serialize>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
        value: &T,
    ) -> Result<(), ConfigRepositoryError> {
        let data = serde_json::to_string(value).map_err(storage_error)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_{table} ({key_col}, data) VALUES (?1, ?2) \
             ON CONFLICT({key_col}) DO UPDATE SET data = excluded.data"
        ))
        .bind(key)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }
}

/// Select the published admin history from the ledger's V1 receipt, then
/// append convergence.
async fn migrate(pool: &EmbeddedPool) -> Result<(), StoreError> {
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
    )
    .bind(format!("{NS}_schema_migrations"))
    .fetch_one(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let v1_checksum: Option<String> = if ledger_exists {
        sqlx::query_scalar(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    let published = selected_admin_bundle(v1_checksum.as_deref())
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let converged =
        converged_admin_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_sqlite_pool::held(runner.run_bundle(&published))
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_sqlite_pool::held(runner.run_bundle(&converged))
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    Ok(())
}

/// One JSON row of `table` by primary key, deserialized; `None` when absent.
async fn fetch_row<'e, T, E>(
    executor: E,
    table: &str,
    key_col: &str,
    key: &str,
) -> Result<Option<T>, ConfigRepositoryError>
where
    T: serde::de::DeserializeOwned,
    E: sqlx::SqliteExecutor<'e>,
{
    let data: Option<String> = sqlx::query_scalar(&format!(
        "SELECT data FROM {NS}_{table} WHERE {key_col} = ?1"
    ))
    .bind(key)
    .fetch_optional(executor)
    .await
    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
    data.map(|data| {
        serde_json::from_str(&data)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    })
    .transpose()
}

/// Every `data` value of `table` ordered by `id`, deserialized.
async fn fetch_all_rows<T: serde::de::DeserializeOwned>(
    pool: &EmbeddedPool,
    table: &str,
) -> Result<Vec<T>, ConfigRepositoryError> {
    let rows: Vec<String> =
        sqlx::query_scalar(&format!("SELECT data FROM {NS}_{table} ORDER BY id"))
            .fetch_all(pool)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
    rows.iter()
        .map(|data| {
            serde_json::from_str(data)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .collect()
}

async fn mutation_pending<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
    id: &str,
) -> Result<bool, ConfigRepositoryError> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {NS}_webhook_mutation WHERE id = ?1)"
    ))
    .bind(id)
    .fetch_one(executor)
    .await
    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
}

fn storage_error(error: impl std::fmt::Display) -> ConfigRepositoryError {
    ConfigRepositoryError::Storage(error.to_string())
}

fn input_storage_error(error: impl std::fmt::Display) -> AgentInputRepositoryError {
    AgentInputRepositoryError::Storage(error.to_string())
}

#[async_trait::async_trait]
impl AgentInputBindingRepository for SqliteAdminStore {
    async fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = agent_input_storage_key(workspace_id, &config.agent_id)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(input_storage_error)?;
        let current: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {NS}_agent_resource WHERE agent_id = ?1"
        ))
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(input_storage_error)?;
        let current = current
            .as_deref()
            .map(serde_json::from_str::<AgentInputConfig>)
            .transpose()
            .map_err(input_storage_error)?;
        let AgentInputAdmission::Write(plan) =
            plan_agent_input_write(workspace_id, current.as_ref(), &config)?
        else {
            return Ok(());
        };
        let data = serde_json::to_string(plan.config()).map_err(input_storage_error)?;
        let statement = match plan.expected_revision() {
            None => format!(
                "INSERT INTO {NS}_agent_resource (agent_id, data) \
                            SELECT ?1, ?2 WHERE ?3 IS NULL ON CONFLICT(agent_id) DO NOTHING"
            ),
            Some(_) => format!(
                "UPDATE {NS}_agent_resource SET data = ?2 \
                               WHERE agent_id = ?1 AND json_extract(data, '$.revision') = ?3"
            ),
        };
        let changed = sqlx::query(&statement)
            .bind(plan.storage_key())
            .bind(data)
            .bind(plan.expected_revision())
            .execute(&mut *tx)
            .await
            .map_err(input_storage_error)?;
        if changed.rows_affected() != 1 {
            return Err(AgentInputRepositoryError::Storage(
                "Agent input plan lost its commit predicate".into(),
            ));
        }
        tx.commit().await.map_err(input_storage_error)
    }
    async fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError> {
        let key = agent_input_storage_key(workspace_id, agent_id)?;
        fetch_row(&self.pool, "agent_resource", "agent_id", &key)
            .await
            .map_err(input_storage_error)?
            .map(|config| validate_stored_agent_inputs(workspace_id, &key, config))
            .transpose()
    }
    async fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError> {
        let prefix = agent_input_workspace_prefix(workspace_id)?;
        let rows: Vec<(String, String)> = sqlx::query_as(&format!(
            "SELECT agent_id, data FROM {NS}_agent_resource ORDER BY agent_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(input_storage_error)?;
        rows.into_iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, data)| {
                let config = serde_json::from_str(&data).map_err(input_storage_error)?;
                validate_stored_agent_inputs(workspace_id, &key, config)
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl InferenceProfileStore for SqliteAdminStore {
    async fn put(
        &self,
        id: String,
        profile: InferenceProfile,
    ) -> Result<(), ConfigRepositoryError> {
        self.put_row("inference_profile", "id", &id, &profile).await
    }
    async fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
        fetch_row(&self.pool, "inference_profile", "id", id).await
    }
}

#[async_trait::async_trait]
impl WebhookStore for SqliteAdminStore {
    async fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError> {
        fetch_row(&self.pool, "webhook", "id", id).await
    }
    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError> {
        // Scan the (low-cardinality) webhook rows and filter by owner in Rust —
        // the generic row helpers key by id; workspace lives inside the JSON.
        Ok(fetch_all_rows::<WebhookEndpointDef>(&self.pool, "webhook")
            .await?
            .into_iter()
            .filter(|definition| definition.workspace_id == workspace_id)
            .collect())
    }
    async fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        if mutation_pending(&mut *tx, &patch.id).await? {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {} has a pending material mutation",
                patch.id
            )));
        }
        let Some(mut definition) =
            fetch_row::<WebhookEndpointDef, _>(&mut *tx, "webhook", "id", &patch.id).await?
        else {
            return Ok(WebhookAuthoringState::Missing);
        };
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
        let data = serde_json::to_string(&definition).map_err(storage_error)?;
        sqlx::query(&format!("UPDATE {NS}_webhook SET data = ?2 WHERE id = ?1"))
            .bind(&patch.id)
            .bind(data)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(WebhookAuthoringState::Updated(definition))
    }

    async fn begin_mutation(
        &self,
        intent: WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        let pending =
            fetch_row::<WebhookMutationIntent, _>(&mut *tx, "webhook_mutation", "id", id).await?;
        if let Some(pending) = pending {
            return if pending == intent {
                Ok(())
            } else {
                Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} already has a pending mutation"
                )))
            };
        }
        let current = fetch_row::<WebhookEndpointDef, _>(&mut *tx, "webhook", "id", id).await?;
        if current != intent.before {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} changed before mutation admission"
            )));
        }
        let data = serde_json::to_string(&intent).map_err(storage_error)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_webhook_mutation(id,data) VALUES (?1,?2)"
        ))
        .bind(id)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    async fn apply_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        let pending =
            fetch_row::<WebhookMutationIntent, _>(&mut *tx, "webhook_mutation", "id", id).await?;
        let current = fetch_row::<WebhookEndpointDef, _>(&mut *tx, "webhook", "id", id).await?;
        if pending.as_ref() != Some(intent) || current != intent.before {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} no longer matches its pending mutation"
            )));
        }
        match &intent.after {
            Some(after) => {
                let data = serde_json::to_string(after).map_err(storage_error)?;
                sqlx::query(&format!("INSERT INTO {NS}_webhook(id,data) VALUES (?1,?2)"))
                    .bind(id)
                    .bind(data)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            None => {
                sqlx::query(&format!("DELETE FROM {NS}_webhook WHERE id = ?1"))
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)
    }

    async fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError> {
        fetch_all_rows(&self.pool, "webhook_mutation").await
    }

    async fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        let Some(pending) =
            fetch_row::<WebhookMutationIntent, _>(&mut *tx, "webhook_mutation", "id", id).await?
        else {
            return Ok(());
        };
        if &pending != intent {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} has a different pending mutation"
            )));
        }
        sqlx::query(&format!("DELETE FROM {NS}_webhook_mutation WHERE id = ?1"))
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn material_refs(
        &self,
    ) -> Result<Vec<awaken_credential_vault::SecretRef>, ConfigRepositoryError> {
        Ok(fetch_all_rows::<WebhookEndpointDef>(&self.pool, "webhook")
            .await?
            .into_iter()
            .map(|definition| definition.secret_ref)
            .collect())
    }

    async fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        if mutation_pending(&mut *tx, id).await? {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} has a pending material mutation"
            )));
        }
        let Some(mut definition) =
            fetch_row::<WebhookEndpointDef, _>(&mut *tx, "webhook", "id", id).await?
        else {
            return Ok(WebhookDeliveryState::Missing);
        };
        let state = definition.record_delivery(outcome, failure_threshold);
        let data = serde_json::to_string(&definition).map_err(storage_error)?;
        sqlx::query(&format!("UPDATE {NS}_webhook SET data = ?2 WHERE id = ?1"))
            .bind(id)
            .bind(data)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{admin_bundle, selected_admin_bundle};
    use awaken_config_resolver::{
        BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, ModelTarget,
        ProfileCandidate, ResourceAccess,
    };
    use awaken_credential_vault::CredentialBinding;
    use std::sync::Arc;

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

    #[tokio::test]
    async fn profile_round_trip_and_overwrite() {
        let store = SqliteAdminStore::open_in_memory().await.unwrap();
        assert!(
            InferenceProfileStore::get(&store, "p1")
                .await
                .unwrap()
                .is_none()
        );
        InferenceProfileStore::put(&store, "p1".into(), profile("m1"))
            .await
            .unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .await
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m1"
        );
        InferenceProfileStore::put(&store, "p1".into(), profile("m2"))
            .await
            .unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .await
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m2"
        );
    }

    #[tokio::test]
    async fn webhook_failure_state_survives_reopen_and_disables_atomically() {
        // Cause/effect graph: C1 durable row; C2 failed delivery; C3 process/store
        // reopen; C4 second failure reaches threshold. Effects E1 count=1 on disk;
        // E2 reopen observes it; E3 row disabled at count=2. Decision table:
        // R1=C1∧C2 -> E1; R2=R1∧C3 -> E2; R3=R2∧C4 -> E3.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        let path = path.to_str().unwrap();
        {
            let store = SqliteAdminStore::open(path).await.unwrap();
            let intent = WebhookMutationIntent::create(WebhookEndpointDef {
                id: "wh".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/hook".into(),
                event_types: vec![],
                disabled: false,
                consecutive_failures: 0,
                secret_ref: awaken_credential_vault::SecretRef("whsec:wh".into()),
            });
            store.begin_mutation(intent.clone()).await.unwrap();
            store.apply_mutation(&intent).await.unwrap();
            store.complete_mutation(&intent).await.unwrap();
            assert_eq!(
                store
                    .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                    .await
                    .unwrap(),
                WebhookDeliveryState::Active {
                    consecutive_failures: 1
                },
                "R1"
            );
        }
        let reopened = SqliteAdminStore::open(path).await.unwrap();
        assert_eq!(
            WebhookStore::get(&reopened, "wh")
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            1,
            "R2"
        );
        assert_eq!(
            reopened
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .await
                .unwrap(),
            WebhookDeliveryState::Disabled {
                consecutive_failures: 2
            },
            "R3"
        );
        assert!(
            WebhookStore::get(&reopened, "wh")
                .await
                .unwrap()
                .unwrap()
                .disabled,
            "R3"
        );
    }

    #[tokio::test]
    async fn webhook_material_intent_survives_reopen_and_fences_other_writers() {
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
            let store = SqliteAdminStore::open(path).await.unwrap();
            store.begin_mutation(intent.clone()).await.unwrap();
        }
        let store = SqliteAdminStore::open(path).await.unwrap();
        assert_eq!(
            store.pending_mutations().await.unwrap(),
            vec![intent.clone()],
            "R1"
        );
        assert!(
            matches!(
                store
                    .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                    .await,
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2"
        );
        assert!(
            matches!(
                store
                    .update_authored(WebhookAuthoringPatch {
                        id: "wh".into(),
                        workspace_id: "ws".into(),
                        url: "https://hooks.example/racing".into(),
                        event_types: vec![],
                        disabled: None,
                    })
                    .await,
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2"
        );
        store.apply_mutation(&intent).await.unwrap();
        store.complete_mutation(&intent).await.unwrap();
        assert!(store.pending_mutations().await.unwrap().is_empty(), "R3");
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
            .await
            .unwrap();
        let updated = store
            .update_authored(WebhookAuthoringPatch {
                id: "wh".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/new".into(),
                event_types: vec!["run.completed".into()],
                disabled: None,
            })
            .await
            .unwrap();
        let WebhookAuthoringState::Updated(updated) = updated else {
            panic!("R4 must update")
        };
        assert_eq!(updated.url, "https://hooks.example/new", "R4");
        assert_eq!(updated.consecutive_failures, 1, "R4");
        assert_eq!(updated.secret_ref, definition.secret_ref, "R4");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_sqlite_authoring_and_delivery_do_not_lose_operational_state() {
        // Cause/effect graph: C1 two independent SQLite connections; C2 authored
        // URL update; C3 failed-delivery increment; C2 and C3 start together.
        // Effects E1 both operations commit in some order, E2 final URL is new,
        // E3 final counter is one, E4 secret ref is unchanged. The IMMEDIATE
        // transaction is the absent/independent-connection serialization edge.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-race.db");
        let path = path.to_str().unwrap().to_string();
        let seed_store = SqliteAdminStore::open(&path).await.unwrap();
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
        seed_store.begin_mutation(intent.clone()).await.unwrap();
        seed_store.apply_mutation(&intent).await.unwrap();
        seed_store.complete_mutation(&intent).await.unwrap();
        drop(seed_store);

        let authoring = SqliteAdminStore::open(&path).await.unwrap();
        let delivery = SqliteAdminStore::open(&path).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let authoring_barrier = barrier.clone();
        let authoring_task = tokio::spawn(async move {
            authoring_barrier.wait().await;
            authoring
                .update_authored(WebhookAuthoringPatch {
                    id: "wh".into(),
                    workspace_id: "ws".into(),
                    url: "https://hooks.example/new".into(),
                    event_types: vec!["run.completed".into()],
                    disabled: None,
                })
                .await
        });
        let delivery_barrier = barrier.clone();
        let delivery_task = tokio::spawn(async move {
            delivery_barrier.wait().await;
            delivery
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
                .await
        });
        barrier.wait().await;
        assert!(authoring_task.await.unwrap().is_ok(), "E1 authoring");
        assert!(delivery_task.await.unwrap().is_ok(), "E1 delivery");

        let final_store = SqliteAdminStore::open(&path).await.unwrap();
        let final_row = WebhookStore::get(&final_store, "wh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.url, "https://hooks.example/new", "E2");
        assert_eq!(final_row.consecutive_failures, 1, "E3");
        assert_eq!(final_row.secret_ref, definition.secret_ref, "E4");
    }

    #[tokio::test]
    async fn current_admin_baseline_applies_once_and_fails_closed_on_drift() {
        // Causes: C1 empty ledger, C2 exact V1 receipt, C3 drifted V1 receipt.
        // Effects: E1 create four active Control tables, E2 replay no SQL, E3
        // reject before schema mutation. Decision table: A1=C1=>E1;
        // A2=C2=>E2; A3=C3=>E3.
        let pool = EmbeddedSqlite::memory().open().await.expect("open sqlite");
        let full = admin_bundle().expect("bundle builds");
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).expect("runner");
        let delta = runner.run_bundle(&full).await.expect("A1 apply baseline");
        assert_eq!(
            delta
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            vec![1],
            "A1/E1"
        );
        assert!(
            runner
                .run_bundle(&full)
                .await
                .expect("A2 replay V1")
                .is_empty(),
            "A2/E2"
        );
        sqlx::query(
            "UPDATE admin_schema_migrations SET checksum = 'drifted' WHERE bundle_id = 'awaken.admin' AND version = 1",
        )
        .execute(&pool)
        .await
        .expect("seed A3 drift");
        assert!(runner.run_bundle(&full).await.is_err(), "A3/E3");
    }

    #[tokio::test]
    async fn published_admin_database_reopens_without_parallel_schema_writes() {
        // Causes: P1 exact expanded V1..V11 with the observed V9 alias;
        // P2 active rows already exist; P3 reopen through the sole Admin adapter.
        // Effects: E1 select expanded history, E2 append only convergence,
        // E3 preserve active rows. Rule R1=P1+P2+P3=>E1+E2+E3; exact replay
        // is covered by the shared runner and schema history table.
        let pool = EmbeddedSqlite::memory()
            .open()
            .await
            .expect("published sqlite");
        let expanded = selected_admin_bundle(Some(
            "a96581cecf572657f5503a0064d76b9b4c6653785d9ff74dd4821752735c604d",
        ))
        .expect("expanded bundle");
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).expect("runner");
        runner
            .run_bundle(&expanded)
            .await
            .expect("seed expanded history");
        sqlx::query(
            "UPDATE admin_schema_migrations SET checksum = ?1
             WHERE bundle_id = ?2 AND version = 9",
        )
        .bind("987ffe8ea131956d8b11c59ec8283d880ed97ce89cf02e9e652f61fbc4478131")
        .bind(BUNDLE_ID)
        .execute(&pool)
        .await
        .expect("seed observed V9 alias");
        sqlx::query("INSERT INTO admin_inference_profile (id, data) VALUES ('profile', '{}')")
            .execute(&pool)
            .await
            .expect("seed active row");
        let store = SqliteAdminStore::over(pool).await.expect("R1 reopen");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM admin_inference_profile WHERE id = 'profile'",
            )
            .fetch_one(&store.pool)
            .await
            .expect("E3 row"),
            1
        );
        drop(store);
    }

    #[tokio::test]
    async fn control_admin_schema_exposes_only_owned_active_adapters() {
        // Cause/effect decision table:
        // R1 active Control profile/input/webhook aggregates and webhook mutation
        // journal -> present.
        // R2 MCP, Memory/Resource catalog, and webhook outbox belong to no Admin
        // adapter -> absent from the current baseline.
        let store = SqliteAdminStore::open_in_memory().await.unwrap();
        let count_tables = |table: &'static str| {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )
            .bind(table)
            .fetch_one(&store.pool)
        };
        for table in [
            "admin_inference_profile",
            "admin_agent_resource",
            "admin_webhook",
            "admin_webhook_mutation",
        ] {
            let count = count_tables(table).await.unwrap();
            assert_eq!(count, 1, "R1: {table}");
        }
        for table in [
            "admin_mcp_server",
            "admin_agent_mcp",
            "admin_memory_store",
            "admin_resource_catalog",
            "admin_webhook_outbox",
        ] {
            let count = count_tables(table).await.unwrap();
            assert_eq!(count, 0, "R2: {table}");
        }
    }

    #[tokio::test]
    async fn agent_resource_binding_round_trips_and_overwrites() {
        let store = SqliteAdminStore::open_in_memory().await.unwrap();
        assert!(
            store
                .get_agent_inputs("workspace-a", "agent-1")
                .await
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
            .await
            .unwrap();
        assert_eq!(
            store
                .get_agent_inputs("workspace-a", "agent-1")
                .await
                .unwrap(),
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
        store.put_agent_inputs("workspace-a", v2).await.unwrap();
        let got = store
            .get_agent_inputs("workspace-a", "agent-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.inputs.len(), 2);
        assert_eq!(got.environment.as_ref().unwrap().revision, 3);
        assert_eq!(got.revision, 2);
        assert!(
            store
                .get_agent_inputs("workspace-a", "agent-2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_agent_inputs("workspace-b", "agent-1")
                .await
                .unwrap()
                .is_none(),
            "Workspace is a mandatory aggregate key"
        );
    }

    #[tokio::test]
    async fn exhausted_agent_input_revision_rejects_and_keeps_the_store_usable() {
        // C1 durable revision is MAX (fixture seeds the unreachable-in-a-short
        // test boundary); C2 replay vs changed content. R1 replay is a no-op;
        // R2 changed MAX -> conflict, preserving bytes and releasing the lock.
        // Regression: unchecked addition panicked while holding the store mutex.
        let store = SqliteAdminStore::open_in_memory().await.unwrap();
        let current = AgentInputConfig {
            agent_id: "agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision: i64::MAX,
        };
        sqlx::query(&format!(
            "INSERT INTO {NS}_agent_resource(agent_id, data) VALUES (?1, ?2)"
        ))
        .bind("workspace\u{1f}agent")
        .bind(serde_json::to_string(&current).unwrap())
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .put_agent_inputs("workspace", current.clone())
            .await
            .unwrap();
        let mut changed = current.clone();
        changed.environment = Some(awaken_config_resolver::AgentEnvironmentBinding {
            environment_id: "changed".into(),
            revision: 1,
        });
        assert_eq!(
            store.put_agent_inputs("workspace", changed).await,
            Err(AgentInputRepositoryError::RevisionConflict {
                current: i64::MAX,
                attempted: i64::MAX
            })
        );
        assert_eq!(
            store.get_agent_inputs("workspace", "agent").await.unwrap(),
            Some(current)
        );
    }

    #[tokio::test]
    async fn rows_survive_a_reopen_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        let path = path.to_str().unwrap();
        {
            let store = SqliteAdminStore::open(path).await.unwrap();
            InferenceProfileStore::put(&store, "p1".into(), profile("m1"))
                .await
                .unwrap();
        }
        // Reopen: the migration is idempotent and the rows are still there.
        let store = SqliteAdminStore::open(path).await.unwrap();
        assert_eq!(
            InferenceProfileStore::get(&store, "p1")
                .await
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "m1"
        );
    }

    #[tokio::test]
    async fn malformed_profile_row_uses_the_repository_error_channel() {
        let store = SqliteAdminStore::open_in_memory().await.unwrap();
        sqlx::query(&format!(
            "INSERT INTO {NS}_inference_profile(id, data) VALUES ('bad', '{{')"
        ))
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(matches!(
            InferenceProfileStore::get(&store, "bad").await,
            Err(ConfigRepositoryError::Storage(_))
        ));
    }
}
