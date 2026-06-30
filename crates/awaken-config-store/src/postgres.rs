//! Postgres config-store adapter under the built-in `config` namespace.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::config::AgentConfig;
use crate::schema::config_bundle;
use crate::store::{ConfigStore, ConfigStoreError, StoredPublication};

/// The config component's table namespace (ADR-0029/ADR-0031). Built in, so the
/// `config_*` tables coexist with the runtime's `runtime_*` in one database.
const NS: &str = "config";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A Postgres-backed [`ConfigStore`].
pub struct PostgresConfigStore {
    pool: PgPool,
}

impl PostgresConfigStore {
    /// Connect and apply the config migrations.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the config migrations under the `config`
    /// namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        let bundle = config_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self { pool })
    }
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

#[async_trait::async_trait]
impl ConfigStore for PostgresConfigStore {
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError> {
        sqlx::query(&format!(
            "INSERT INTO {NS}_agent (id, data) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET data = excluded.data"
        ))
        .bind(&config.id)
        .bind(Json(config))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        let row = sqlx::query(&format!("SELECT data FROM {NS}_agent WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(reject)?;
        match row {
            Some(row) => {
                let Json(config): Json<AgentConfig> = row.try_get("data").map_err(reject)?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        sqlx::query(&format!(
            "INSERT INTO {NS}_publication (fingerprint, agent_id, state, record) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (fingerprint) DO NOTHING"
        ))
        .bind(&publication.fingerprint)
        .bind(&publication.agent_id)
        .bind(publication.state.as_str())
        .bind(Json(publication))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let row = sqlx::query(&format!(
            "SELECT record FROM {NS}_publication WHERE fingerprint = $1"
        ))
        .bind(fingerprint)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        match row {
            Some(row) => {
                let Json(record): Json<StoredPublication> =
                    row.try_get("record").map_err(reject)?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}
