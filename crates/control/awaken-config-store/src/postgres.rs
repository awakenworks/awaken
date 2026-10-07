// @formal entrypoint audit.business_action_follows_audit_record
// @formal entrypoint audit.business_commit_and_audit_transition_are_atomic
// @formal entrypoint audit.call_identity_is_stable
// @formal entrypoint audit.stable_call_replay_does_not_repeat_business_write
// @formal entrypoint config.generation_is_monotonic
// @formal entrypoint config.publication_revision_is_scoped_by_execution_target_and_exact_replay
// @formal entrypoint config.stale_writer_never_applies
// @formal entrypoint config_activation.stale_publication_never_installs
// @formal entrypoint management_http.crash_preserves_pending_audit
// @formal entrypoint management_http.successful_write_marks_audit_committed
// @formal entrypoint resource_binding.draft_audit_and_effect_are_atomic
// @formal entrypoint resource_binding.pending_effect_survives_until_readback
// @formal entrypoint resource_binding.replay_is_idempotent_and_identity_stable
//! Postgres config-store adapter under the built-in `config` namespace.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::stored_u64::StoredU64;
use awaken_tenancy::ScopeId;

use crate::codec::{attach_publication_row, decode_publication_value};
use crate::management_effect_retirement_is_admitted;
use crate::schema::{BUNDLE_ID, converged_config_bundle, selected_config_bundle};
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError,
    ConfigWrite, DEFAULT_SCOPE, ManagementAuditAdmission, ManagementAuditEntry,
    ManagementAuditRecord, ManagementEffect, ManagementEffectReceipt, PublicationRevisionDecision,
    PublicationWrite, ScopedConfigRegistry, StoredPublication, publication_revision_decision,
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
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

    /// Inject immutable history for codec, recovery, and quarantine tests.
    /// Production publication has only the source-fenced trait method.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_publication_history_for_test(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        insert_publication(&self.pool, scope, publication).await
    }
}

async fn insert_publication<'e, E>(
    executor: E,
    scope: &'e ScopeId,
    publication: &'e StoredPublication,
) -> Result<(), ConfigStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(&format!(
        "INSERT INTO {NS}_publication (fingerprint, agent_id, state, record, scope_id, created_at, order_verified) \
         VALUES ($1, $2, $3, $4, $5, clock_timestamp(), $6) \
         ON CONFLICT (scope_id, fingerprint) DO NOTHING"
    ))
    .bind(&publication.fingerprint)
    .bind(&publication.agent_id)
    .bind(publication.state.as_str())
    .bind(Json(publication))
    .bind(&scope.0)
    .bind(publication.publication_order_verified)
    .execute(executor)
    .await
    .map_err(reject)?;
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
        selected_config_bundle(v1_checksum.as_deref())
            .map_err(|error| StoreError::Schema(error.to_string()))?,
        converged_config_bundle().map_err(|error| StoreError::Schema(error.to_string()))?,
    ))
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

fn domain_generation(value: i64) -> Result<u64, ConfigStoreError> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(reject)
}

/// The one Config-store transaction advisory-lock primitive. Both authoring
/// revision and publication commit ordering use this existing PostgreSQL key
/// vocabulary; hash collisions only over-serialize unrelated keys.
async fn lock_advisory_pair(
    conn: &mut sqlx::PgConnection,
    left: &str,
    right: &str,
) -> Result<(), ConfigStoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(left)
        .bind(right)
        .execute(conn)
        .await
        .map_err(reject)?;
    Ok(())
}

/// Serialize every Config writer for one `(scope, id)` before it observes the
/// row. Hash collisions only serialize unrelated keys; they cannot merge their
/// separately predicated rows.
async fn lock_config_revision(
    conn: &mut sqlx::PgConnection,
    scope: &ScopeId,
    id: &str,
) -> Result<(), ConfigStoreError> {
    lock_advisory_pair(conn, &scope.0, id).await
}

/// Serialize the sole production publication transaction per execution
/// Workspace before it takes any Agent row lock. The lock remains held through
/// commit, so `seq` allocation order is commit order among these writes.
/// Hash collisions only serialize unrelated Workspaces. Acquiring this first
/// avoids a lock-order cycle with Config's `(scope, id)` writer lock.
async fn lock_publication_commit_order(
    conn: &mut sqlx::PgConnection,
    execution_workspace: &str,
) -> Result<(), ConfigStoreError> {
    lock_advisory_pair(
        conn,
        "awaken.config.publication.commit_order",
        execution_workspace,
    )
    .await
}

/// Shared by unconditional, ordinary CAS and audited writes. A missing row is
/// not locked by SELECT FOR UPDATE, so creation remains an atomic insert-only
/// statement; a positive expected revision is strictly update-only.
async fn write_config_revision(
    conn: &mut sqlx::PgConnection,
    scope: &ScopeId,
    config: &AgentConfig,
    plan: crate::ConfigRevisionWritePlan,
) -> Result<Option<u64>, ConfigStoreError> {
    let statement = if plan.kind == crate::ConfigRevisionWriteKind::Insert {
        format!(
            "INSERT INTO {NS}_agent (id, data, scope_id, generation) \
                 SELECT $1, $2, $3, $4 WHERE $5 = 0 \
                 ON CONFLICT(scope_id, id) DO NOTHING RETURNING generation"
        )
    } else {
        format!(
            "UPDATE {NS}_agent SET data = $2, generation = $4 \
                 WHERE id = $1 AND scope_id = $3 AND generation = $5 RETURNING generation"
        )
    };
    sqlx::query_scalar::<_, i64>(&statement)
        .bind(&config.id)
        .bind(Json(config))
        .bind(&scope.0)
        .bind(plan.next.database_value())
        .bind(plan.expected.database_value())
        .fetch_optional(conn)
        .await
        .map_err(reject)?
        .map(domain_generation)
        .transpose()
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
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_config_revision(&mut tx, scope, &config.id).await?;
        let current_revision = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2 FOR UPDATE"
        ))
        .bind(&config.id)
        .bind(&scope.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?
        .map(domain_generation)
        .transpose()?;
        let plan = match crate::ConfigRevisionWritePlan::advance_current(current_revision)
            .map_err(reject)?
        {
            crate::ConfigRevisionAdmission::Apply(plan) => plan,
            crate::ConfigRevisionAdmission::Conflict { .. } => {
                return Err(ConfigStoreError(
                    "config generation is present at invalid zero".into(),
                ));
            }
        };
        if write_config_revision(&mut tx, scope, config, plan)
            .await?
            .is_none()
        {
            return Err(ConfigStoreError(
                "config generation transition lost its exclusive writer".into(),
            ));
        }
        tx.commit().await.map_err(reject)?;
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
        let (audit_tool, audit_key) = audit.identity();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_config_revision(&mut tx, scope, &config.id).await?;
        let existing = sqlx::query(&format!(
            "SELECT record, business_committed FROM {NS}_management_audit \
             WHERE scope_id = $1 AND call_id = $2 AND tool = $3 FOR UPDATE"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(audit_tool)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let existing = existing
            .map(|row| {
                let Json(record): Json<ManagementAuditRecord> =
                    row.try_get("record").map_err(reject)?;
                let state: i64 = row.try_get("business_committed").map_err(reject)?;
                Ok::<_, ConfigStoreError>((record, state))
            })
            .transpose()?;
        if audit.admit_write(existing.as_ref().map(|(record, state)| (record, *state)))?
            == ManagementAuditAdmission::Replay
        {
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
        let plan =
            match crate::ConfigRevisionWritePlan::admit(current_revision, expected_generation)
                .map_err(reject)?
            {
                crate::ConfigRevisionAdmission::Apply(plan) => plan,
                crate::ConfigRevisionAdmission::Conflict { current_revision } => {
                    return Ok(AuditedConfigWrite::Conflict { current_revision });
                }
            };
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
            let pending = existing
                .map(|row| {
                    let Json(effect): Json<ManagementEffect> =
                        row.try_get("payload").map_err(reject)?;
                    Ok::<_, ConfigStoreError>(effect)
                })
                .transpose()?;
            if effect.requires_enqueue(pending.as_ref())? {
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
        let applied = write_config_revision(&mut tx, scope, &config, plan).await?;
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
             WHERE scope_id = $1 AND call_id = $2 AND tool = $3"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(audit_tool)
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
            effect.validate_index(&kind, &key)?;
            Ok(effect)
        })
        .collect()
    }

    async fn complete_management_effect_scoped(
        &self,
        receipt: &ManagementEffectReceipt,
    ) -> Result<bool, ConfigStoreError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let effect = receipt.effect();
        let row = sqlx::query(&format!(
            "SELECT payload FROM {NS}_management_effect \
             WHERE scope_id = $1 AND kind = $2 AND effect_key = $3 FOR UPDATE"
        ))
        .bind(&receipt.scope().0)
        .bind(effect.kind())
        .bind(effect.key())
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let pending = row
            .map(|row| {
                let Json(pending): Json<ManagementEffect> =
                    row.try_get("payload").map_err(reject)?;
                Ok::<_, ConfigStoreError>(pending)
            })
            .transpose()?;
        if let Some(pending) = &pending {
            pending.validate_index(effect.kind(), effect.key())?;
        }
        if !management_effect_retirement_is_admitted(receipt, pending.as_ref()) {
            return Ok(false);
        }
        let removed = sqlx::query(&format!(
            "DELETE FROM {NS}_management_effect \
             WHERE scope_id = $1 AND kind = $2 AND effect_key = $3"
        ))
        .bind(&receipt.scope().0)
        .bind(effect.kind())
        .bind(effect.key())
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(removed.rows_affected() == 1)
    }

    async fn record_management_audit_scoped(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let (audit_tool, audit_key) = audit.identity();
        let inserted = sqlx::query(&format!(
            "INSERT INTO {NS}_management_audit (scope_id, call_id, tool, record) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (scope_id, tool, call_id) DO NOTHING RETURNING call_id"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(audit_tool)
        .bind(Json(audit))
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        if inserted.is_some() {
            return Ok(AuditedConfigWrite::Applied);
        }
        let row = sqlx::query(&format!(
            "SELECT record FROM {NS}_management_audit WHERE scope_id = $1 AND call_id = $2 AND tool = $3"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(audit_tool)
        .fetch_one(&self.pool)
        .await
        .map_err(reject)?;
        let Json(existing): Json<ManagementAuditRecord> = row.try_get("record").map_err(reject)?;
        audit.admit_write(Some((&existing, 0)))?;
        Ok(AuditedConfigWrite::Replayed)
    }

    async fn get_management_audit_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        let audit_key = call_id;
        let row = sqlx::query(&format!(
            "SELECT record, business_committed FROM {NS}_management_audit \
             WHERE scope_id = $1 AND call_id = $2 AND tool = $3"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(tool)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        row.map(|row| {
            let Json(record): Json<ManagementAuditRecord> =
                row.try_get("record").map_err(reject)?;
            let business_committed: i64 = row.try_get("business_committed").map_err(reject)?;
            ManagementAuditEntry::from_stored(record, business_committed)
        })
        .transpose()
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        let audit_key = call_id;
        let changed = sqlx::query(&format!(
            "UPDATE {NS}_management_audit SET business_committed = 1 \
             WHERE scope_id = $1 AND call_id = $2 AND tool = $3"
        ))
        .bind(&scope.0)
        .bind(audit_key)
        .bind(tool)
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
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_config_revision(&mut tx, scope, &config.id).await?;
        let current_revision = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT generation FROM {NS}_agent WHERE id = $1 AND scope_id = $2 FOR UPDATE"
        ))
        .bind(&config.id)
        .bind(&scope.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?
        .map(domain_generation)
        .transpose()?;
        let plan =
            match crate::ConfigRevisionWritePlan::admit(current_revision, expected_generation)
                .map_err(reject)?
            {
                crate::ConfigRevisionAdmission::Apply(plan) => plan,
                crate::ConfigRevisionAdmission::Conflict { current_revision } => {
                    return Ok(ConfigWrite::Conflict { current_revision });
                }
            };
        if let Some(revision) = write_config_revision(&mut tx, scope, config, plan).await? {
            tx.commit().await.map_err(reject)?;
            return Ok(ConfigWrite::Applied { revision });
        }
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

    async fn put_publication_if_source_current_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        lock_publication_commit_order(&mut tx, publication.execution_workspace.as_str()).await?;
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
        if let crate::PublicationSourceAdmission::Conflict { current_revision } =
            crate::PublicationSourceAdmission::new(current_revision, publication.source_revision)
        {
            tx.rollback().await.map_err(reject)?;
            return Ok(PublicationWrite::Conflict { current_revision });
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
            return Ok(PublicationWrite::Conflict { current_revision });
        }
        let mut committed = publication.clone();
        committed.publication_order_verified = true;
        insert_publication(&mut *tx, scope, &committed).await?;
        tx.commit().await.map_err(reject)?;
        Ok(PublicationWrite::Committed {
            source_revision: publication.source_revision,
        })
    }

    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let row = sqlx::query(&format!(
            "SELECT record, seq, order_verified, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS published_at_unix_ms \
             FROM {NS}_publication WHERE fingerprint = $1 AND scope_id = $2"
        ))
        .bind(fingerprint)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        match row {
            Some(row) => {
                let Json(value): Json<serde_json::Value> = row.try_get("record").map_err(reject)?;
                let sequence: i64 = row.try_get("seq").map_err(reject)?;
                let order_verified: bool = row.try_get("order_verified").map_err(reject)?;
                let published_at: i64 = row.try_get("published_at_unix_ms").map_err(reject)?;
                let publication = decode_publication_value(value, scope).map_err(reject)?;
                Ok(Some(attach_publication_row(
                    publication,
                    sequence,
                    published_at,
                    order_verified,
                )?))
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
        // the in-memory store). The monotonic `seq` identity column (migration
        // V0005) is the Postgres analogue of SQLite's `rowid`: wall-clock
        // rollback must not reorder publication recovery.
        let rows = sqlx::query(&format!(
            "SELECT record, seq, order_verified, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS published_at_unix_ms \
             FROM {NS}_publication \
             WHERE scope_id = $1 AND state = 'published' ORDER BY seq ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Json(value): Json<serde_json::Value> = row.try_get("record").map_err(reject)?;
            let sequence: i64 = row.try_get("seq").map_err(reject)?;
            let order_verified: bool = row.try_get("order_verified").map_err(reject)?;
            let published_at: i64 = row.try_get("published_at_unix_ms").map_err(reject)?;
            let publication = decode_publication_value(value, scope).map_err(reject)?;
            out.push(attach_publication_row(
                publication,
                sequence,
                published_at,
                order_verified,
            )?);
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

    async fn put_publication_if_source_current(
        &self,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
        self.put_publication_if_source_current_scoped(&ScopeId::from(DEFAULT_SCOPE), publication)
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
