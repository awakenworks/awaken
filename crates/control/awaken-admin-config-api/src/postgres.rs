//! Postgres adapter (feature `postgres`, ADR-0043) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`PostgresAdminStore`] serves the same three sync store ports the sqlite
//! backend does from a single connection pool.
//!
//! The repository ports are synchronous but fallible: SQL and JSON failures
//! cross the repository boundary as `ConfigRepositoryError`. `sqlx` is async-only,
//! so this backend
//! owns a dedicated single-worker Tokio runtime and drives each short query to
//! completion on it via [`block`], which runs the future on a fresh OS thread so
//! it is safe to call from *inside* the server's request-handler runtime (a
//! nested `block_on` would panic otherwise). Each bridged future owns its inputs
//! (the pool is an `Arc`, cloned in; params are owned), so there are no
//! cross-thread borrow puzzles. Admin writes are low-frequency authoring
//! operations, so the per-call thread hop is negligible.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use tokio::runtime::{Builder, Handle, Runtime};

use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, AgentInputRepositoryError,
    ConfigRepositoryError, InferenceProfile, InferenceProfileStore, WebhookAuthoringPatch,
    WebhookAuthoringState, WebhookDeliveryOutcome, WebhookDeliveryState, WebhookEndpointDef,
    WebhookMutationIntent, WebhookStore, validate_agent_input_revision,
};
use awaken_store_runtime::block_on_owned_runtime as block;

use crate::schema::{BUNDLE_ID, converged_admin_bundle, selected_admin_bundle};

/// The admin component's table namespace (its bundle prefix).
pub(crate) const NS: &str = "admin";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("runtime: {0}")]
    Runtime(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("schema: {0}")]
    Schema(String),
}

/// A Postgres-backed store for the admin-plane aggregates, implementing all four
/// sync store ports. Wrap in `Arc` and clone into each `AdminState` slot so the
/// ports share one pool.
pub struct PostgresAdminStore {
    pub(crate) pool: PgPool,
    pub(crate) handle: Handle,
    /// The store owns its runtime; `Option` so [`Drop`] can shut it down in the
    /// background (dropping a runtime from within another runtime would panic).
    rt: Option<Runtime>,
}

impl PostgresAdminStore {
    /// Connect and apply the admin migrations under the `admin` namespace. Sync so
    /// it composes with the sync ports; the pool lives on this store's own runtime.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_with_mode(url, true)
    }

    /// Connect to an already-migrated admin schema without executing DDL.
    pub fn connect_existing(url: &str) -> Result<Self, StoreError> {
        Self::connect_with_mode(url, false)
    }

    fn connect_with_mode(url: &str, migrate_schema: bool) -> Result<Self, StoreError> {
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| StoreError::Runtime(err.to_string()))?;
        let handle = rt.handle().clone();
        let url = url.to_string();
        let pool = block(&handle, move || async move {
            let pool = PgPool::connect(&url)
                .await
                .map_err(|err| StoreError::Connect(err.to_string()))?;
            if migrate_schema {
                migrate(&pool).await?;
            } else {
                verify(&pool).await?;
            }
            Ok::<_, StoreError>(pool)
        })?;
        Ok(Self {
            pool,
            handle,
            rt: Some(rt),
        })
    }

    fn put_json<T: serde::Serialize>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
        value: &T,
    ) -> Result<(), ConfigRepositoryError> {
        let sql = format!(
            "INSERT INTO {NS}_{table} ({key_col}, data) VALUES ($1, $2) \
             ON CONFLICT ({key_col}) DO UPDATE SET data = excluded.data"
        );
        let pool = self.pool.clone();
        let key = key.to_string();
        let data = serde_json::to_value(value)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        block(&self.handle, move || async move {
            sqlx::query(&sql)
                .bind(key)
                .bind(Json(data))
                .execute(&pool)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            Ok(())
        })
    }

    fn get_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
    ) -> Result<Option<T>, ConfigRepositoryError> {
        let sql = format!("SELECT data FROM {NS}_{table} WHERE {key_col} = $1");
        let pool = self.pool.clone();
        let key = key.to_string();
        block(&self.handle, move || async move {
            let row = sqlx::query(&sql)
                .bind(key)
                .fetch_optional(&pool)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            row.map(|row| {
                row.try_get::<Json<T>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
            })
            .transpose()
        })
    }

    /// All JSON rows of `table` ordered by `key_col` (the in-memory/sqlite stores'
    /// sorted-list contract).
    fn list_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        key_col: &str,
    ) -> Result<Vec<T>, ConfigRepositoryError> {
        let sql = format!("SELECT data FROM {NS}_{table} ORDER BY {key_col}");
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let rows = sqlx::query(&sql)
                .fetch_all(&pool)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            rows.into_iter()
                .map(|row| {
                    row.try_get::<Json<T>, _>("data")
                        .map(|Json(value)| value)
                        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
                })
                .collect()
        })
    }
}

impl Drop for PostgresAdminStore {
    fn drop(&mut self) {
        // A Tokio runtime must not be dropped from within another runtime (it would
        // panic on the blocking shutdown). `shutdown_background` hands the teardown
        // off so dropping the store from an async context (e.g. server shutdown) is
        // safe.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// Apply the `admin` migration bundle to `pool` (idempotent; the runner records
/// applied versions in `admin_schema_migrations`).
async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let (published, converged) = selected_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&published)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&converged)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    Ok(())
}

async fn verify(pool: &PgPool) -> Result<(), StoreError> {
    let (published, converged) = selected_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| StoreError::Schema(error.to_string()))?;
    runner
        .verify_bundle(&published)
        .await
        .map_err(|error| StoreError::Schema(error.to_string()))?;
    runner
        .verify_bundle(&converged)
        .await
        .map_err(|error| StoreError::Schema(error.to_string()))?;
    Ok(())
}

async fn selected_bundles(
    pool: &PgPool,
) -> Result<
    (
        awaken_scoped_migration::MigrationBundle,
        awaken_scoped_migration::MigrationBundle,
    ),
    StoreError,
> {
    let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(pool)
        .await
        .map_err(|error| StoreError::Schema(error.to_string()))?;
    let v1_checksum: Option<String> = if ledger.is_some() {
        sqlx::query_scalar(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = $1 AND version = 1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| StoreError::Schema(error.to_string()))?
    } else {
        None
    };
    Ok((
        selected_admin_bundle(v1_checksum.as_deref())
            .map_err(|error| StoreError::Schema(error.to_string()))?,
        converged_admin_bundle().map_err(|error| StoreError::Schema(error.to_string()))?,
    ))
}

/// Serialize every mutation for one webhook id, including the absent-row create
/// case where `SELECT .. FOR UPDATE` has no tuple to lock.
async fn lock_webhook_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
) -> Result<(), ConfigRepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
    Ok(())
}

impl AgentInputBindingRepository for PostgresAdminStore {
    fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = format!("{workspace_id}\u{1f}{}", config.agent_id);
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
            let row = sqlx::query(&format!(
                "SELECT data FROM {NS}_agent_resource WHERE agent_id = $1 FOR UPDATE"
            ))
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
            let current = row
                .map(|row| {
                    let Json(value): Json<AgentInputConfig> = row
                        .try_get("data")
                        .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
                    Ok::<_, AgentInputRepositoryError>(value)
                })
                .transpose()?;
            if !validate_agent_input_revision(current.as_ref(), &config)? {
                return Ok(());
            }
            let data = serde_json::to_value(&config)
                .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
            sqlx::query(&format!(
                "INSERT INTO {NS}_agent_resource (agent_id, data) VALUES ($1, $2) \
                 ON CONFLICT (agent_id) DO UPDATE SET data = excluded.data"
            ))
            .bind(key)
            .bind(Json(data))
            .execute(&mut *tx)
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
        })
    }
    fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError> {
        let key = format!("{workspace_id}\u{1f}{agent_id}");
        self.get_json("agent_resource", "agent_id", &key)
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
    }
    fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError> {
        let prefix = format!("{workspace_id}\u{1f}");
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let rows = sqlx::query(&format!(
                "SELECT agent_id, data FROM {NS}_agent_resource ORDER BY agent_id COLLATE \"C\""
            ))
            .fetch_all(&pool)
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
            rows.into_iter()
                .filter_map(|row| {
                    let key = row.try_get::<String, _>("agent_id").ok()?;
                    key.starts_with(&prefix).then_some(row)
                })
                .map(|row| {
                    let Json(config): Json<AgentInputConfig> = row
                        .try_get("data")
                        .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
                    Ok(config)
                })
                .collect()
        })
    }
}

impl WebhookStore for PostgresAdminStore {
    fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError> {
        self.get_json("webhook", "id", id)
    }
    fn list(&self, workspace_id: &str) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError> {
        // Reuse the sorted list bridge, then fence by owner in Rust (webhook rows are
        // low-cardinality; workspace lives inside the JSON, not a column).
        Ok(self
            .list_json::<WebhookEndpointDef>("webhook", "id")?
            .into_iter()
            .filter(|d| d.workspace_id == workspace_id)
            .collect())
    }
    fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError> {
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            lock_webhook_id(&mut tx, &patch.id).await?;
            let pending = sqlx::query(&format!(
                "SELECT 1 FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
            ))
            .bind(&patch.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            if pending.is_some() {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {} has a pending material mutation",
                    patch.id
                )));
            }
            let row = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook WHERE id = $1 FOR UPDATE"
            ))
            .bind(&patch.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            let Some(row) = row else {
                return Ok(WebhookAuthoringState::Missing);
            };
            let Json(mut definition): Json<WebhookEndpointDef> = row
                .try_get("data")
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
            let data = serde_json::to_value(&definition)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            sqlx::query(&format!("UPDATE {NS}_webhook SET data = $2 WHERE id = $1"))
                .bind(&patch.id)
                .bind(Json(data))
                .execute(&mut *tx)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            Ok(WebhookAuthoringState::Updated(definition))
        })
    }

    fn begin_mutation(&self, intent: WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let data = serde_json::to_value(&intent)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            lock_webhook_id(&mut tx, &id).await?;
            let pending = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            if let Some(row) = pending {
                let Json(pending): Json<WebhookMutationIntent> = row
                    .try_get("data")
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                return if pending == intent {
                    Ok(())
                } else {
                    Err(ConfigRepositoryError::MutationConflict(format!(
                        "webhook {id} already has a pending mutation"
                    )))
                };
            }
            let row = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            let current = row
                .map(|row| {
                    row.try_get::<Json<WebhookEndpointDef>, _>("data")
                        .map(|Json(value)| value)
                        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
                })
                .transpose()?;
            if current != intent.before {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} changed before mutation admission"
                )));
            }
            sqlx::query(&format!(
                "INSERT INTO {NS}_webhook_mutation(id,data) VALUES ($1,$2)"
            ))
            .bind(&id)
            .bind(Json(data))
            .execute(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
    }

    fn apply_mutation(&self, intent: &WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let intent = intent.clone();
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            lock_webhook_id(&mut tx, &id).await?;
            let pending = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?
            .map(|row| {
                row.try_get::<Json<WebhookMutationIntent>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
            })
            .transpose()?;
            let current = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?
            .map(|row| {
                row.try_get::<Json<WebhookEndpointDef>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
            })
            .transpose()?;
            if pending.as_ref() != Some(&intent) || current != intent.before {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} no longer matches its pending mutation"
                )));
            }
            match &intent.after {
                Some(after) => {
                    let data = serde_json::to_value(after)
                        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                    sqlx::query(&format!("INSERT INTO {NS}_webhook(id,data) VALUES ($1,$2)"))
                        .bind(&id)
                        .bind(Json(data))
                        .execute(&mut *tx)
                        .await
                        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                }
                None => {
                    sqlx::query(&format!("DELETE FROM {NS}_webhook WHERE id = $1"))
                        .bind(&id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                }
            }
            tx.commit()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
    }

    fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError> {
        self.list_json("webhook_mutation", "id")
    }

    fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let pool = self.pool.clone();
        let id = intent.id()?.to_string();
        let intent = intent.clone();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            lock_webhook_id(&mut tx, &id).await?;
            let pending = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?
            .map(|row| {
                row.try_get::<Json<WebhookMutationIntent>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
            })
            .transpose()?;
            match pending {
                None => return Ok(()),
                Some(pending) if pending == intent => {}
                Some(_) => {
                    return Err(ConfigRepositoryError::MutationConflict(format!(
                        "webhook {id} has a different pending mutation"
                    )));
                }
            }
            sqlx::query(&format!("DELETE FROM {NS}_webhook_mutation WHERE id = $1"))
                .bind(&id)
                .execute(&mut *tx)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
    }

    fn material_refs(
        &self,
    ) -> Result<Vec<awaken_credential_vault::SecretRef>, ConfigRepositoryError> {
        Ok(self
            .list_json::<WebhookEndpointDef>("webhook", "id")?
            .into_iter()
            .map(|definition| definition.secret_ref)
            .collect())
    }

    fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError> {
        let pool = self.pool.clone();
        let id = id.to_string();
        block(&self.handle, move || async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            lock_webhook_id(&mut tx, &id).await?;
            let pending = sqlx::query(&format!(
                "SELECT 1 FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            if pending.is_some() {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} has a pending material mutation"
                )));
            }
            let row = sqlx::query(&format!(
                "SELECT data FROM {NS}_webhook WHERE id = $1 FOR UPDATE"
            ))
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            let Some(row) = row else {
                return Ok(WebhookDeliveryState::Missing);
            };
            let Json(mut definition): Json<WebhookEndpointDef> = row
                .try_get("data")
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            let state = definition.record_delivery(outcome, failure_threshold);
            let data = serde_json::to_value(&definition)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            sqlx::query(&format!("UPDATE {NS}_webhook SET data = $2 WHERE id = $1"))
                .bind(id)
                .bind(Json(data))
                .execute(&mut *tx)
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            Ok(state)
        })
    }
}

impl InferenceProfileStore for PostgresAdminStore {
    fn put(&self, id: String, profile: InferenceProfile) -> Result<(), ConfigRepositoryError> {
        self.put_json("inference_profile", "id", &id, &profile)
    }
    fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
        self.get_json("inference_profile", "id", id)
    }
}
