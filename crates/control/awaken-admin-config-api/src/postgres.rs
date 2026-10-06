// @formal entrypoint agent_inputs.rejected_revision_does_not_mutate
//! Postgres adapter (feature `postgres`, ADR-0043) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`PostgresAdminStore`] serves the same store ports the sqlite backend
//! does from a single connection pool.
//!
//! The repository ports are asynchronous and fallible: SQL and JSON failures
//! cross the repository boundary as `ConfigRepositoryError`, and every port
//! awaits its queries on the caller's runtime.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use awaken_config_resolver::{
    AgentInputAdmission, AgentInputBindingRepository, AgentInputConfig, AgentInputRepositoryError,
    ConfigRepositoryError, InferenceProfile, InferenceProfileStore, WebhookAuthoringPatch,
    WebhookAuthoringState, WebhookDeliveryOutcome, WebhookDeliveryState, WebhookEndpointDef,
    WebhookMutationIntent, WebhookStore, agent_input_storage_key, agent_input_workspace_prefix,
    plan_agent_input_write, validate_stored_agent_inputs,
};

use crate::schema::{BUNDLE_ID, converged_admin_bundle, selected_admin_bundle};

mod mcp;

/// The admin component's table namespace (its bundle prefix).
pub(crate) const NS: &str = "admin";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("schema: {0}")]
    Schema(String),
}

/// A Postgres-backed store for the admin-plane aggregates, implementing all four
/// store ports. Wrap in `Arc` and clone into each `AdminState` slot so the
/// ports share one pool.
pub struct PostgresAdminStore {
    pub(crate) pool: PgPool,
}

impl PostgresAdminStore {
    /// Connect and apply the admin migrations under the `admin` namespace.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_with_mode(url, true).await
    }

    /// Connect to an already-migrated admin schema without executing DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, StoreError> {
        Self::connect_with_mode(url, false).await
    }

    async fn connect_with_mode(url: &str, migrate_schema: bool) -> Result<Self, StoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        if migrate_schema {
            migrate(&pool).await?;
        } else {
            verify(&pool).await?;
        }
        Ok(Self { pool })
    }

    async fn put_json<T: serde::Serialize>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
        value: &T,
    ) -> Result<(), ConfigRepositoryError> {
        let data = serde_json::to_value(value)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_{table} ({key_col}, data) VALUES ($1, $2) \
             ON CONFLICT ({key_col}) DO UPDATE SET data = excluded.data"
        ))
        .bind(key)
        .bind(Json(data))
        .execute(&self.pool)
        .await
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        Ok(())
    }
}

/// One JSON row of `table` by primary key, deserialized; `None` when absent.
async fn fetch_json<T: serde::de::DeserializeOwned>(
    pool: &PgPool,
    table: &str,
    key_col: &str,
    key: &str,
) -> Result<Option<T>, ConfigRepositoryError> {
    let row = sqlx::query(&format!(
        "SELECT data FROM {NS}_{table} WHERE {key_col} = $1"
    ))
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
    row.map(|row| {
        row.try_get::<Json<T>, _>("data")
            .map(|Json(value)| value)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    })
    .transpose()
}

/// All JSON rows of `table` ordered by `key_col`.
async fn fetch_all_json<T: serde::de::DeserializeOwned>(
    pool: &PgPool,
    table: &str,
    key_col: &str,
) -> Result<Vec<T>, ConfigRepositoryError> {
    let rows = sqlx::query(&format!("SELECT data FROM {NS}_{table} ORDER BY {key_col}"))
        .fetch_all(pool)
        .await
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
    rows.into_iter()
        .map(|row| {
            row.try_get::<Json<T>, _>("data")
                .map(|Json(value)| value)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .collect()
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

/// Serialize aggregate mutations including absent-row creation, where SELECT
/// FOR UPDATE has no tuple to lock. Webhooks retain their existing lock identity;
/// Agent inputs use a table-qualified key. Hash collisions only over-serialize.
async fn lock_aggregate_id(
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

#[async_trait::async_trait]
impl AgentInputBindingRepository for PostgresAdminStore {
    async fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = agent_input_storage_key(workspace_id, &config.agent_id)?;
        let workspace_id = workspace_id.to_owned();
        let pool = &self.pool;
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, &format!("{NS}_agent_resource:{key}"))
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
        let AgentInputAdmission::Write(plan) =
            plan_agent_input_write(&workspace_id, current.as_ref(), &config)?
        else {
            return Ok(());
        };
        let data = serde_json::to_value(plan.config())
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        let statement = match plan.expected_revision() {
            None => format!(
                "INSERT INTO {NS}_agent_resource (agent_id, data) \
                            SELECT $1, $2 WHERE $3::bigint IS NULL ON CONFLICT(agent_id) DO NOTHING"
            ),
            Some(_) => format!(
                "UPDATE {NS}_agent_resource SET data = $2 \
                               WHERE agent_id = $1 AND (data->>'revision')::bigint = $3"
            ),
        };
        let changed = sqlx::query(&statement)
            .bind(plan.storage_key())
            .bind(Json(data))
            .bind(plan.expected_revision())
            .execute(&mut *tx)
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        if changed.rows_affected() != 1 {
            return Err(AgentInputRepositoryError::Storage(
                "Agent input plan lost its commit predicate".into(),
            ));
        }
        tx.commit()
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))
    }
    async fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError> {
        let key = agent_input_storage_key(workspace_id, agent_id)?;
        fetch_json(&self.pool, "agent_resource", "agent_id", &key)
            .await
            .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?
            .map(|config| validate_stored_agent_inputs(workspace_id, &key, config))
            .transpose()
    }
    async fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError> {
        let prefix = agent_input_workspace_prefix(workspace_id)?;
        let workspace_id = workspace_id.to_owned();
        let pool = &self.pool;
        let rows = sqlx::query(&format!(
            "SELECT agent_id, data FROM {NS}_agent_resource ORDER BY agent_id COLLATE \"C\""
        ))
        .fetch_all(pool)
        .await
        .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let key = row
                    .try_get::<String, _>("agent_id")
                    .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
                Ok((key, row))
            })
            .filter(|row: &Result<_, AgentInputRepositoryError>| {
                row.as_ref()
                    .map_or(true, |(key, _)| key.starts_with(&prefix))
            })
            .map(|row| {
                let (key, row) = row?;
                let Json(config): Json<AgentInputConfig> = row
                    .try_get("data")
                    .map_err(|error| AgentInputRepositoryError::Storage(error.to_string()))?;
                validate_stored_agent_inputs(&workspace_id, &key, config)
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl WebhookStore for PostgresAdminStore {
    async fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError> {
        fetch_json(&self.pool, "webhook", "id", id).await
    }
    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError> {
        // Reuse the sorted list query, then fence by owner in Rust (webhook rows are
        // low-cardinality; workspace lives inside the JSON, not a column).
        Ok(
            fetch_all_json::<WebhookEndpointDef>(&self.pool, "webhook", "id")
                .await?
                .into_iter()
                .filter(|d| d.workspace_id == workspace_id)
                .collect(),
        )
    }
    async fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, &patch.id).await?;
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
    }

    async fn begin_mutation(
        &self,
        intent: WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let data = serde_json::to_value(&intent)
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, &id).await?;
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
    }

    async fn apply_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, id).await?;
        let pending = sqlx::query(&format!(
            "SELECT data FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
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
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?
        .map(|row| {
            row.try_get::<Json<WebhookEndpointDef>, _>("data")
                .map(|Json(value)| value)
                .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
        })
        .transpose()?;
        if pending.as_ref() != Some(intent) || current != intent.before {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} no longer matches its pending mutation"
            )));
        }
        match &intent.after {
            Some(after) => {
                let data = serde_json::to_value(after)
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
                sqlx::query(&format!("INSERT INTO {NS}_webhook(id,data) VALUES ($1,$2)"))
                    .bind(id)
                    .bind(Json(data))
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            }
            None => {
                sqlx::query(&format!("DELETE FROM {NS}_webhook WHERE id = $1"))
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
            }
        }
        tx.commit()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    }

    async fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError> {
        fetch_all_json(&self.pool, "webhook_mutation", "id").await
    }

    async fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, id).await?;
        let pending = sqlx::query(&format!(
            "SELECT data FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
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
            Some(pending) if &pending == intent => {}
            Some(_) => {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} has a different pending mutation"
                )));
            }
        }
        sqlx::query(&format!("DELETE FROM {NS}_webhook_mutation WHERE id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))
    }

    async fn material_refs(
        &self,
    ) -> Result<Vec<awaken_credential_vault::SecretRef>, ConfigRepositoryError> {
        Ok(
            fetch_all_json::<WebhookEndpointDef>(&self.pool, "webhook", "id")
                .await?
                .into_iter()
                .map(|definition| definition.secret_ref)
                .collect(),
        )
    }

    async fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ConfigRepositoryError::Storage(error.to_string()))?;
        lock_aggregate_id(&mut tx, id).await?;
        let pending = sqlx::query(&format!(
            "SELECT 1 FROM {NS}_webhook_mutation WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
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
        .bind(id)
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
    }
}

#[async_trait::async_trait]
impl InferenceProfileStore for PostgresAdminStore {
    async fn put(
        &self,
        id: String,
        profile: InferenceProfile,
    ) -> Result<(), ConfigRepositoryError> {
        self.put_json("inference_profile", "id", &id, &profile)
            .await
    }
    async fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
        fetch_json(&self.pool, "inference_profile", "id", id).await
    }
}
