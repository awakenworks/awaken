//! Postgres config-store adapter under the built-in `config` namespace.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use awaken_tenancy::ScopeId;

use crate::config::AgentConfig;
use crate::schema::config_bundle;
use crate::store::{
    ConfigRegistry, ConfigStoreError, DEFAULT_SCOPE, ScopedConfigRegistry, StoredPublication,
};

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

/// A Postgres-backed [`ConfigRegistry`].
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
impl ScopedConfigRegistry for PostgresConfigStore {
    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        // The `ON CONFLICT … WHERE` guard makes a cross-scope write a no-op, so a
        // workspace cannot clobber another's agent by id.
        sqlx::query(&format!(
            "INSERT INTO {NS}_agent (id, data, scope_id) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET data = excluded.data \
             WHERE {NS}_agent.scope_id = excluded.scope_id"
        ))
        .bind(&config.id)
        .bind(Json(config))
        .bind(&scope.0)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError> {
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_agent WHERE id = $1 AND scope_id = $2"
        ))
        .bind(id)
        .bind(&scope.0)
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

    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_agent WHERE scope_id = $1 ORDER BY id ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut configs = Vec::with_capacity(rows.len());
        for row in rows {
            let Json(config): Json<AgentConfig> = row.try_get("data").map_err(reject)?;
            configs.push(config);
        }
        Ok(configs)
    }

    async fn put_publication_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        sqlx::query(&format!(
            "INSERT INTO {NS}_publication (fingerprint, agent_id, state, record, scope_id) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (fingerprint) DO NOTHING"
        ))
        .bind(&publication.fingerprint)
        .bind(&publication.agent_id)
        .bind(publication.state.as_str())
        .bind(Json(publication))
        .bind(&scope.0)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let row = sqlx::query(&format!(
            "SELECT record FROM {NS}_publication WHERE fingerprint = $1 AND scope_id = $2"
        ))
        .bind(fingerprint)
        .bind(&scope.0)
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

    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
        // Postgres is a durable store, so it must reload published publications for
        // warm-install (the default trait impl returns empty and is only right for
        // the in-memory store). Oldest-first by insertion (`created_at` ascending, the
        // Postgres analogue of SQLite's `rowid`), so a map keyed by agent keeps the
        // latest publication per agent.
        let rows = sqlx::query(&format!(
            "SELECT record FROM {NS}_publication \
             WHERE scope_id = $1 AND state = 'published' ORDER BY created_at ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Json(record): Json<StoredPublication> = row.try_get("record").map_err(reject)?;
            out.push(record);
        }
        Ok(out)
    }
}

/// The scope-free [`ConfigRegistry`] over Postgres operates in the seeded
/// [`DEFAULT_SCOPE`] — byte-identical to the pre-tenancy behavior. Multi-tenant
/// callers wrap with [`crate::ScopedConfig`] bound to the request's scope.
#[async_trait::async_trait]
impl ConfigRegistry for PostgresConfigStore {
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError> {
        self.put_config_scoped(&ScopeId::from(DEFAULT_SCOPE), config)
            .await
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.get_config_scoped(&ScopeId::from(DEFAULT_SCOPE), id)
            .await
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        self.list_configs_scoped(&ScopeId::from(DEFAULT_SCOPE))
            .await
    }

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        self.put_publication_scoped(&ScopeId::from(DEFAULT_SCOPE), publication)
            .await
    }

    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        self.get_publication_scoped(&ScopeId::from(DEFAULT_SCOPE), fingerprint)
            .await
    }
}
