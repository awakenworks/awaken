//! Postgres config-store adapter under the built-in `config` namespace.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use awaken_store_runtime::StoredU64;
use awaken_tenancy::ScopeId;

use crate::codec::decode_publication_value;
use crate::schema::{BUNDLE_ID, converged_config_bundle, selected_config_bundle};
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError,
    ConfigWrite, DEFAULT_SCOPE, ManagementAuditEntry, ManagementAuditRecord, ManagementEffect,
    PublicationRevisionDecision, ScopedConfigRegistry, StoredPublication,
    publication_revision_decision,
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
    #[error("schema: {0}")]
    Schema(String),
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

    /// Connect to an already-migrated config store without changing schema.
    ///
    /// Managed deployments run migrations as a separate operational phase. This
    /// constructor verifies the scoped ledger and fails closed when a migration
    /// is pending or drifted, so an application process never becomes a second
    /// migration runner.
    pub async fn connect_existing(url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool: apply the config migrations under the `config`
    /// namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        let (published, converged) = selected_bundles(&pool).await?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
        runner
            .run_bundle(&published)
            .await
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        runner
            .run_bundle(&converged)
            .await
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        Ok(Self { pool })
    }

    /// Build from a pool whose config migrations were applied out of process.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        let (published, converged) = selected_bundles(&pool).await?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|error| StoreError::Schema(error.to_string()))?;
        runner
            .verify_bundle(&published)
            .await
            .map_err(|error| StoreError::Schema(error.to_string()))?;
        runner
            .verify_bundle(&converged)
            .await
            .map_err(|error| StoreError::Schema(error.to_string()))?;
        Ok(Self { pool })
    }
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
        selected_config_bundle(v1_checksum.as_deref())
            .map_err(|error| StoreError::Schema(error.to_string()))?,
        converged_config_bundle().map_err(|error| StoreError::Schema(error.to_string()))?,
    ))
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

fn database_generation(value: u64) -> Result<i64, ConfigStoreError> {
    StoredU64::try_from(value)
        .map(StoredU64::database_value)
        .map_err(reject)
}

fn domain_generation(value: i64) -> Result<u64, ConfigStoreError> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(reject)
}

#[async_trait::async_trait]
impl ScopedConfigRegistry for PostgresConfigStore {
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT scope_id FROM {NS}_agent ORDER BY scope_id ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("scope_id")
                    .map(ScopeId::from)
                    .map_err(reject)
            })
            .collect()
    }

    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        // Agent identity is `(scope_id, id)`: portable Agent ids may be reused
        // across scopes without allowing either owner to clobber the other.
        sqlx::query(&format!(
            "INSERT INTO {NS}_agent (id, data, scope_id, generation) VALUES ($1, $2, $3, 1) \
             ON CONFLICT (scope_id, id) DO UPDATE SET data = excluded.data, \
             generation = {NS}_agent.generation + 1"
        ))
        .bind(&config.id)
        .bind(Json(config))
        .bind(&scope.0)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn put_config_with_audit_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_generation: u64,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        self.put_config_with_audit_effect_scoped(scope, config, expected_generation, audit, None)
            .await
    }

    async fn put_config_with_audit_effect_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_generation: u64,
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let existing = sqlx::query(&format!(
            "SELECT record, business_committed FROM {NS}_management_audit \
             WHERE scope_id = $1 AND call_id = $2 FOR UPDATE"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let replayed = if let Some(row) = existing {
            let Json(existing): Json<ManagementAuditRecord> =
                row.try_get("record").map_err(reject)?;
            if existing != *audit {
                return Err(ConfigStoreError(
                    "stable audit call id was reused with different content".into(),
                ));
            }
            let committed: i64 = row.try_get("business_committed").map_err(reject)?;
            committed != 0
        } else {
            return Err(ConfigStoreError(
                "audited config transaction requires a pre-recorded management audit".into(),
            ));
        };
        if replayed {
            return Ok(AuditedConfigWrite::Replayed);
        }
        let current = sqlx::query(&format!(
            "SELECT data, generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2 FOR UPDATE"
        ))
        .bind(&config.id)
        .bind(&scope.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let current_revision = current
            .as_ref()
            .map(|row| row.try_get::<i64, _>("generation").map_err(reject))
            .transpose()?
            .map(domain_generation)
            .transpose()?;
        if current_revision.unwrap_or(0) != expected_generation {
            return Ok(AuditedConfigWrite::Conflict { current_revision });
        }
        let current_config = current
            .map(|row| {
                let Json(config): Json<AgentConfig> = row.try_get("data").map_err(reject)?;
                Ok::<_, ConfigStoreError>(config)
            })
            .transpose()?;
        let config = config
            .canonicalize_mutable_authoring_against(current_config.as_ref())
            .map_err(reject)?;
        if let Some(effect) = effect {
            let existing = sqlx::query(&format!(
                "SELECT payload FROM {NS}_management_effect \
                 WHERE scope_id = $1 AND kind = $2 AND effect_key = $3 FOR UPDATE"
            ))
            .bind(&scope.0)
            .bind(effect.kind())
            .bind(effect.key())
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?;
            if let Some(row) = existing {
                let Json(existing_effect): Json<ManagementEffect> =
                    row.try_get("payload").map_err(reject)?;
                if existing_effect != *effect {
                    return Err(ConfigStoreError(
                        "stable management effect key was reused with different content".into(),
                    ));
                }
            } else {
                sqlx::query(&format!(
                    "INSERT INTO {NS}_management_effect \
                     (scope_id, kind, effect_key, payload) VALUES ($1, $2, $3, $4)"
                ))
                .bind(&scope.0)
                .bind(effect.kind())
                .bind(effect.key())
                .bind(Json(effect))
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
        }
        let applied = sqlx::query_scalar::<_, i64>(&format!(
            "INSERT INTO {NS}_agent (id, data, scope_id, generation) VALUES ($1, $2, $3, 1) \
             ON CONFLICT (scope_id, id) DO UPDATE SET data = excluded.data, \
             generation = {NS}_agent.generation + 1 \
             WHERE {NS}_agent.generation = $4 RETURNING generation"
        ))
        .bind(&config.id)
        .bind(Json(&config))
        .bind(&scope.0)
        .bind(database_generation(expected_generation)?)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if applied.is_none() {
            let current_revision = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2"
            ))
            .bind(&config.id)
            .bind(&scope.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?
            .map(domain_generation)
            .transpose()?;
            return Ok(AuditedConfigWrite::Conflict { current_revision });
        }
        sqlx::query(&format!(
            "UPDATE {NS}_management_audit SET business_committed = 1 \
             WHERE scope_id = $1 AND call_id = $2"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(AuditedConfigWrite::Applied)
    }

    async fn pending_management_effects_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<ManagementEffect>, ConfigStoreError> {
        sqlx::query(&format!(
            "SELECT kind, effect_key, payload FROM {NS}_management_effect \
             WHERE scope_id = $1 ORDER BY created_at, kind, effect_key"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?
        .into_iter()
        .map(|row| {
            let kind: String = row.try_get("kind").map_err(reject)?;
            let key: String = row.try_get("effect_key").map_err(reject)?;
            let Json(effect): Json<ManagementEffect> = row.try_get("payload").map_err(reject)?;
            if kind != effect.kind() || key != effect.key() {
                return Err(ConfigStoreError(
                    "management effect index does not match its typed payload".into(),
                ));
            }
            Ok(effect)
        })
        .collect()
    }

    async fn complete_management_effect_scoped(
        &self,
        scope: &ScopeId,
        kind: &str,
        key: &str,
    ) -> Result<(), ConfigStoreError> {
        sqlx::query(&format!(
            "DELETE FROM {NS}_management_effect \
             WHERE scope_id = $1 AND kind = $2 AND effect_key = $3"
        ))
        .bind(&scope.0)
        .bind(kind)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn record_management_audit_scoped(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        let inserted = sqlx::query(&format!(
            "INSERT INTO {NS}_management_audit (scope_id, call_id, record) VALUES ($1, $2, $3) \
             ON CONFLICT (scope_id, call_id) DO NOTHING RETURNING call_id"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .bind(Json(audit))
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        if inserted.is_some() {
            return Ok(AuditedConfigWrite::Applied);
        }
        let row = sqlx::query(&format!(
            "SELECT record FROM {NS}_management_audit WHERE scope_id = $1 AND call_id = $2"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .fetch_one(&self.pool)
        .await
        .map_err(reject)?;
        let Json(existing): Json<ManagementAuditRecord> = row.try_get("record").map_err(reject)?;
        if existing == *audit {
            Ok(AuditedConfigWrite::Replayed)
        } else {
            Err(ConfigStoreError(
                "stable audit call id was reused with different content".into(),
            ))
        }
    }

    async fn get_management_audit_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        let audit_key = format!("{tool}:{call_id}");
        let row = sqlx::query(&format!(
            "SELECT record, business_committed FROM {NS}_management_audit \
             WHERE scope_id = $1 AND call_id = $2"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        row.map(|row| {
            let Json(record): Json<ManagementAuditRecord> =
                row.try_get("record").map_err(reject)?;
            let business_committed: i64 = row.try_get("business_committed").map_err(reject)?;
            Ok(ManagementAuditEntry {
                record,
                business_committed: business_committed != 0,
            })
        })
        .transpose()
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        let audit_key = format!("{tool}:{call_id}");
        let changed = sqlx::query(&format!(
            "UPDATE {NS}_management_audit SET business_committed = 1 \
             WHERE scope_id = $1 AND call_id = $2"
        ))
        .bind(&scope.0)
        .bind(&audit_key)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        if changed.rows_affected() != 1 {
            return Err(ConfigStoreError(
                "management audit completion target was not found".into(),
            ));
        }
        Ok(())
    }

    async fn put_config_if_revision_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let applied = sqlx::query_scalar::<_, i64>(&format!(
            "INSERT INTO {NS}_agent (id, data, scope_id, generation) VALUES ($1, $2, $3, 1) \
             ON CONFLICT (scope_id, id) DO UPDATE SET data = excluded.data, \
             generation = {NS}_agent.generation + 1 \
             WHERE {NS}_agent.generation = $4 RETURNING generation"
        ))
        .bind(&config.id)
        .bind(Json(config))
        .bind(&scope.0)
        .bind(database_generation(expected_generation)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        if let Some(generation) = applied {
            return Ok(ConfigWrite::Applied {
                revision: domain_generation(generation)?,
            });
        }
        let current_revision = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2"
        ))
        .bind(&config.id)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?
        .map(domain_generation)
        .transpose()?;
        Ok(ConfigWrite::Conflict { current_revision })
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

    async fn get_config_revision_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        let row = sqlx::query(&format!(
            "SELECT a.data, a.generation, \
             (EXTRACT(EPOCH FROM first.created_at) * 1000)::BIGINT AS created_at_unix_ms, \
             (EXTRACT(EPOCH FROM current.created_at) * 1000)::BIGINT AS updated_at_unix_ms \
             FROM {NS}_agent a \
             JOIN {NS}_agent_revision first ON first.scope_id = a.scope_id \
               AND first.id = a.id AND first.generation = 1 \
             JOIN {NS}_agent_revision current ON current.scope_id = a.scope_id \
               AND current.id = a.id AND current.generation = a.generation \
             WHERE a.id = $1 AND a.scope_id = $2"
        ))
        .bind(id)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        match row {
            Some(row) => {
                let Json(config): Json<AgentConfig> = row.try_get("data").map_err(reject)?;
                let revision: i64 = row.try_get("generation").map_err(reject)?;
                let created_at_unix_ms: i64 = row.try_get("created_at_unix_ms").map_err(reject)?;
                let updated_at_unix_ms: i64 = row.try_get("updated_at_unix_ms").map_err(reject)?;
                Ok(Some(AgentConfigRevision {
                    config,
                    revision: domain_generation(revision)?,
                    created_at_unix_ms: Some(u64::try_from(created_at_unix_ms).map_err(reject)?),
                    updated_at_unix_ms: Some(u64::try_from(updated_at_unix_ms).map_err(reject)?),
                }))
            }
            None => Ok(None),
        }
    }

    async fn list_config_revisions_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT data, generation, \
             (EXTRACT(EPOCH FROM MIN(created_at) OVER ()) * 1000)::BIGINT AS created_at_unix_ms, \
             (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS updated_at_unix_ms \
             FROM {NS}_agent_revision \
             WHERE scope_id = $1 AND id = $2 ORDER BY generation ASC"
        ))
        .bind(&scope.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut revisions = Vec::with_capacity(rows.len());
        for row in rows {
            let Json(config): Json<AgentConfig> = row.try_get("data").map_err(reject)?;
            let revision: i64 = row.try_get("generation").map_err(reject)?;
            let created_at_unix_ms: i64 = row.try_get("created_at_unix_ms").map_err(reject)?;
            let updated_at_unix_ms: i64 = row.try_get("updated_at_unix_ms").map_err(reject)?;
            revisions.push(AgentConfigRevision {
                config,
                revision: u64::try_from(revision).map_err(reject)?,
                created_at_unix_ms: Some(u64::try_from(created_at_unix_ms).map_err(reject)?),
                updated_at_unix_ms: Some(u64::try_from(updated_at_unix_ms).map_err(reject)?),
            });
        }
        Ok(revisions)
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
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (scope_id, fingerprint) DO NOTHING"
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

    async fn put_publication_if_config_revision_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current_revision = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2 FOR UPDATE"
        ))
        .bind(&publication.agent_id)
        .bind(&scope.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?
        .map(domain_generation)
        .transpose()?;
        if current_revision != Some(expected_generation) {
            tx.rollback().await.map_err(reject)?;
            return Ok(ConfigWrite::Conflict { current_revision });
        }
        let existing = sqlx::query(&format!(
            "SELECT record FROM {NS}_publication WHERE scope_id = $1 AND agent_id = $2 FOR UPDATE"
        ))
        .bind(&scope.0)
        .bind(&publication.agent_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?;
        let existing = existing
            .into_iter()
            .map(|row| {
                let Json(value): Json<serde_json::Value> = row.try_get("record").map_err(reject)?;
                decode_publication_value(value, scope).map_err(reject)
            })
            .collect::<Result<Vec<StoredPublication>, _>>()?;
        let decision = publication_revision_decision(
            publication.execution_workspace.as_str(),
            publication.source_revision,
            publication.fingerprint.as_str(),
            existing.iter().map(|existing| {
                (
                    existing.execution_workspace.as_str(),
                    existing.source_revision,
                    existing.fingerprint.as_str(),
                )
            }),
        );
        if decision == PublicationRevisionDecision::Conflict {
            tx.rollback().await.map_err(reject)?;
            return Ok(ConfigWrite::Conflict { current_revision });
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_publication (fingerprint, agent_id, state, record, scope_id) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (scope_id, fingerprint) DO NOTHING"
        ))
        .bind(&publication.fingerprint)
        .bind(&publication.agent_id)
        .bind(publication.state.as_str())
        .bind(Json(publication))
        .bind(&scope.0)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(ConfigWrite::Applied {
            revision: expected_generation,
        })
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
                let Json(value): Json<serde_json::Value> = row.try_get("record").map_err(reject)?;
                Ok(Some(
                    decode_publication_value(value, scope).map_err(reject)?,
                ))
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
        // the in-memory store). Oldest-first by insertion, with the monotonic `seq`
        // identity column (migration V0005) as a deterministic tie-break for rows
        // sharing a `created_at` — the Postgres analogue of SQLite's `rowid ASC`, so a
        // map keyed by agent keeps the same latest-publication-per-agent on both.
        let rows = sqlx::query(&format!(
            "SELECT record FROM {NS}_publication \
             WHERE scope_id = $1 AND state = 'published' ORDER BY created_at ASC, seq ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Json(value): Json<serde_json::Value> = row.try_get("record").map_err(reject)?;
            out.push(decode_publication_value(value, scope).map_err(reject)?);
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

    async fn put_config_if_revision(
        &self,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.put_config_if_revision_scoped(
            &ScopeId::from(DEFAULT_SCOPE),
            config,
            expected_generation,
        )
        .await
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.get_config_scoped(&ScopeId::from(DEFAULT_SCOPE), id)
            .await
    }

    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        self.get_config_revision_scoped(&ScopeId::from(DEFAULT_SCOPE), id)
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

    async fn put_publication_if_config_revision(
        &self,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.put_publication_if_config_revision_scoped(
            &ScopeId::from(DEFAULT_SCOPE),
            publication,
            expected_generation,
        )
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
