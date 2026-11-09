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
//! SQLite config-store adapter under the built-in `config` namespace.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

mod personal_selection;

use async_trait::async_trait;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::sqlite::SqliteConnection;

use awaken_agent_contract::ScopeId;

use crate::codec::{attach_publication_row, decode_publication};
use crate::management_effect_retirement_is_admitted;
use crate::schema::{BUNDLE_ID, converged_config_bundle, selected_config_bundle};
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError,
    ConfigWrite, DEFAULT_SCOPE, ManagementAuditAdmission, ManagementAuditEntry,
    ManagementAuditRecord, ManagementEffect, ManagementEffectReceipt, PublicationRevisionDecision,
    PublicationWrite, ScopedConfigRegistry, StoredPublication, publication_revision_decision,
};

/// The config component's table namespace (ADR-0029/ADR-0031). Built in.
const NS: &str = "config";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed [`ConfigRegistry`].
pub struct SqliteConfigStore {
    pool: EmbeddedPool,
}

impl SqliteConfigStore {
    /// Open (or create) a database file and apply the config migrations.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::file(path)
            .open()
            .await
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_pool(pool).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::memory()
            .open()
            .await
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: EmbeddedPool) -> Result<Self, StoreError> {
        let ledger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        )
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(&pool)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let v1_checksum: Option<String> = if ledger_exists {
            sqlx::query_scalar(&format!(
                "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
            ))
            .bind(BUNDLE_ID)
            .fetch_optional(&pool)
            .await
            .map_err(|error| StoreError::Migrate(error.to_string()))?
        } else {
            None
        };
        let published = selected_config_bundle(v1_checksum.as_deref())
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let converged =
            converged_config_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        awaken_sqlite_pool::held(runner.run_bundle(&published))
            .await
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        awaken_sqlite_pool::held(runner.run_bundle(&converged))
            .await
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
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
        let state = publication.state.as_str();
        let record = serde_json::to_string(publication).map_err(reject)?;
        let mut conn = self.pool.acquire().await.map_err(reject)?;
        insert_publication(
            &mut conn,
            NS,
            &publication.fingerprint,
            &publication.agent_id,
            state,
            &record,
            &scope.0,
        )
        .await
    }
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

/// SQLite has no unsigned 64-bit column: a stored generation or instant is an
/// `INTEGER` that must be non-negative to be a domain value.
fn unsigned(value: i64) -> Result<u64, ConfigStoreError> {
    u64::try_from(value).map_err(reject)
}

async fn insert_publication(
    conn: &mut SqliteConnection,
    p: &str,
    fingerprint: &str,
    agent_id: &str,
    state: &str,
    record: &str,
    scope: &str,
) -> Result<(), ConfigStoreError> {
    sqlx::query(&format!(
        "INSERT INTO {p}_publication (fingerprint, agent_id, state, record, scope_id) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(scope_id, fingerprint) DO NOTHING"
    ))
    .bind(fingerprint)
    .bind(agent_id)
    .bind(state)
    .bind(record)
    .bind(scope)
    .execute(&mut *conn)
    .await
    .map_err(reject)?;
    Ok(())
}

/// The current generation of one Agent row, if it exists.
async fn current_generation(
    conn: &mut SqliteConnection,
    p: &str,
    scope: &str,
    id: &str,
) -> Result<Option<u64>, ConfigStoreError> {
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
    ))
    .bind(id)
    .bind(scope)
    .fetch_optional(&mut *conn)
    .await
    .map_err(reject)?
    .map(unsigned)
    .transpose()
}

/// The single CAS write seam for plain and audited writes. The caller owns
/// any wider audit/effect transaction; this statement owns the atomic revision
/// predicate, including the absent-row case (zero means create, never update).
async fn write_config_revision(
    conn: &mut SqliteConnection,
    p: &str,
    scope: &str,
    id: &str,
    data: &str,
    plan: crate::ConfigRevisionWritePlan,
) -> Result<Option<u64>, ConfigStoreError> {
    let statement = if plan.kind == crate::ConfigRevisionWriteKind::Insert {
        format!(
            "INSERT INTO {p}_agent (id, data, scope_id, generation) \
                 SELECT ?1, ?2, ?3, ?4 WHERE ?5 = 0 \
                 ON CONFLICT(scope_id, id) DO NOTHING RETURNING generation"
        )
    } else {
        format!(
            "UPDATE {p}_agent SET data = ?2, generation = ?4 \
                 WHERE id = ?1 AND scope_id = ?3 AND generation = ?5 RETURNING generation"
        )
    };
    sqlx::query_scalar::<_, i64>(&statement)
        .bind(id)
        .bind(data)
        .bind(scope)
        .bind(plan.next.database_value())
        .bind(plan.expected.database_value())
        .fetch_optional(&mut *conn)
        .await
        .map_err(reject)?
        .map(unsigned)
        .transpose()
}

#[async_trait]
impl ScopedConfigRegistry for SqliteConfigStore {
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError> {
        let p = NS;
        let scopes = sqlx::query_scalar::<_, String>(&format!(
            "SELECT DISTINCT scope_id FROM {p}_agent ORDER BY scope_id ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        Ok(scopes.into_iter().map(ScopeId::from).collect())
    }

    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        let p = NS;
        let id = config.id.as_str();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.as_str();
        // Agent identity is `(scope_id, id)`: another scope may reuse the
        // same portable Agent id without reading or clobbering this row.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(reject)?;
        let current_revision = current_generation(&mut tx, p, scope, id).await?;
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
        if write_config_revision(&mut tx, p, scope, id, &data, plan)
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
        let p = NS;
        let id = config.id.as_str();
        let scope = scope.0.as_str();
        let (tool, call_id) = audit.identity();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(reject)?;
        let existing: Option<(String, i64)> = sqlx::query_as(&format!(
            "SELECT record, business_committed FROM {p}_management_audit \
             WHERE scope_id = ?1 AND call_id = ?2 AND tool = ?3"
        ))
        .bind(scope)
        .bind(call_id)
        .bind(tool)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let existing = existing
            .map(|(record, state)| {
                serde_json::from_str::<ManagementAuditRecord>(&record)
                    .map(|record| (record, state))
                    .map_err(reject)
            })
            .transpose()?;
        if audit.admit_write(existing.as_ref().map(|(record, state)| (record, *state)))?
            == ManagementAuditAdmission::Replay
        {
            tx.commit().await.map_err(reject)?;
            return Ok(AuditedConfigWrite::Replayed);
        }
        let current: Option<(String, i64)> = sqlx::query_as(&format!(
            "SELECT data, generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
        ))
        .bind(id)
        .bind(scope)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let current = current
            .map(|(data, revision)| Ok::<_, ConfigStoreError>((data, unsigned(revision)?)))
            .transpose()?;
        let current_revision = current.as_ref().map(|(_, revision)| *revision);
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
            .as_ref()
            .map(|(data, _)| serde_json::from_str::<AgentConfig>(data).map_err(reject))
            .transpose()?;
        let config = config
            .clone()
            .canonicalize_mutable_authoring_against(current_config.as_ref())
            .map_err(reject)?;
        let data = serde_json::to_string(&config).map_err(reject)?;
        if let Some(effect) = effect {
            let effect_payload = serde_json::to_string(effect).map_err(reject)?;
            let existing_payload: Option<String> = sqlx::query_scalar(&format!(
                "SELECT payload FROM {p}_management_effect \
                 WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
            ))
            .bind(scope)
            .bind(effect.kind())
            .bind(effect.key())
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?;
            let pending = existing_payload
                .as_deref()
                .map(serde_json::from_str::<ManagementEffect>)
                .transpose()
                .map_err(reject)?;
            if effect.requires_enqueue(pending.as_ref())? {
                sqlx::query(&format!(
                    "INSERT INTO {p}_management_effect (scope_id, kind, effect_key, payload) VALUES (?1, ?2, ?3, ?4)"
                ))
                .bind(scope)
                .bind(effect.kind())
                .bind(effect.key())
                .bind(effect_payload)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
        }
        if write_config_revision(&mut tx, p, scope, id, &data, plan)
            .await?
            .is_none()
        {
            let current_revision = current_generation(&mut tx, p, scope, id).await?;
            return Ok(AuditedConfigWrite::Conflict { current_revision });
        }
        sqlx::query(&format!(
            "UPDATE {p}_management_audit SET business_committed = 1 \
             WHERE scope_id = ?1 AND call_id = ?2 AND tool = ?3"
        ))
        .bind(scope)
        .bind(call_id)
        .bind(tool)
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
        let p = NS;
        let rows: Vec<(String, String, String)> = sqlx::query_as(&format!(
            "SELECT kind, effect_key, payload FROM {p}_management_effect \
             WHERE scope_id = ?1 ORDER BY created_at, kind, effect_key"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|(kind, key, payload)| {
                let effect: ManagementEffect = serde_json::from_str(&payload).map_err(reject)?;
                effect.validate_index(&kind, &key)?;
                Ok(effect)
            })
            .collect()
    }

    async fn complete_management_effect_scoped(
        &self,
        receipt: &ManagementEffectReceipt,
    ) -> Result<bool, ConfigStoreError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let effect = receipt.effect();
        let scope = &receipt.scope().0;
        let payload: Option<String> = sqlx::query_scalar(&format!(
            "SELECT payload FROM {p}_management_effect \
              WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
        ))
        .bind(scope)
        .bind(effect.kind())
        .bind(effect.key())
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let pending = payload
            .as_deref()
            .map(serde_json::from_str::<ManagementEffect>)
            .transpose()
            .map_err(reject)?;
        if let Some(pending) = &pending {
            pending.validate_index(effect.kind(), effect.key())?;
        }
        if !management_effect_retirement_is_admitted(receipt, pending.as_ref()) {
            return Ok(false);
        }
        // The same transaction protects the exact observation through DELETE.
        // A concurrent SQLite writer makes this upgrade fail, never delete a
        // newer payload using an earlier unfenced observation.
        let removed = sqlx::query(&format!(
            "DELETE FROM {p}_management_effect \
              WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
        ))
        .bind(scope)
        .bind(effect.kind())
        .bind(effect.key())
        .execute(&mut *tx)
        .await
        .map_err(reject)?
        .rows_affected();
        tx.commit().await.map_err(reject)?;
        Ok(removed == 1)
    }

    async fn record_management_audit_scoped(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let p = NS;
        let scope = scope.0.as_str();
        let data = serde_json::to_string(audit).map_err(reject)?;
        let (tool, call_id) = audit.identity();
        // The unique tuple arbitrates across connections, not a process-local
        // read-before-insert race. Exact retry compares domain data, not JSON text.
        let inserted = sqlx::query(&format!(
            "INSERT INTO {p}_management_audit (scope_id, tool, call_id, record) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (scope_id, tool, call_id) DO NOTHING"
        ))
        .bind(scope)
        .bind(tool)
        .bind(call_id)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(reject)?
        .rows_affected();
        if inserted == 1 {
            return Ok(AuditedConfigWrite::Applied);
        }
        let existing: String = sqlx::query_scalar(&format!(
            "SELECT record FROM {p}_management_audit \
             WHERE scope_id = ?1 AND tool = ?2 AND call_id = ?3"
        ))
        .bind(scope)
        .bind(tool)
        .bind(call_id)
        .fetch_one(&self.pool)
        .await
        .map_err(reject)?;
        let existing = serde_json::from_str(&existing).map_err(reject)?;
        audit.admit_write(Some((&existing, 0)))?;
        Ok(AuditedConfigWrite::Replayed)
    }

    async fn get_management_audit_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        let p = NS;
        let row: Option<(String, i64)> = sqlx::query_as(&format!(
            "SELECT record, business_committed FROM {p}_management_audit \
             WHERE scope_id = ?1 AND call_id = ?2 AND tool = ?3"
        ))
        .bind(&scope.0)
        .bind(call_id)
        .bind(tool)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        row.map(|(record, business_committed)| {
            ManagementAuditEntry::from_stored(
                serde_json::from_str(&record).map_err(reject)?,
                business_committed,
            )
        })
        .transpose()
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        let p = NS;
        let changed = sqlx::query(&format!(
            "UPDATE {p}_management_audit SET business_committed = 1 \
             WHERE scope_id = ?1 AND call_id = ?2 AND tool = ?3"
        ))
        .bind(&scope.0)
        .bind(call_id)
        .bind(tool)
        .execute(&self.pool)
        .await
        .map_err(reject)?
        .rows_affected();
        if changed != 1 {
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
        let p = NS;
        let id = config.id.as_str();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.as_str();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(reject)?;
        let current_revision = current_generation(&mut tx, p, scope, id).await?;
        let plan =
            match crate::ConfigRevisionWritePlan::admit(current_revision, expected_generation)
                .map_err(reject)?
            {
                crate::ConfigRevisionAdmission::Apply(plan) => plan,
                crate::ConfigRevisionAdmission::Conflict { current_revision } => {
                    return Ok(ConfigWrite::Conflict { current_revision });
                }
            };
        if let Some(revision) = write_config_revision(&mut tx, p, scope, id, &data, plan).await? {
            tx.commit().await.map_err(reject)?;
            return Ok(ConfigWrite::Applied { revision });
        }
        let current_revision = current_generation(&mut tx, p, scope, id).await?;
        Ok(ConfigWrite::Conflict { current_revision })
    }

    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError> {
        let p = NS;
        let data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
        ))
        .bind(id)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        data.map(|s| serde_json::from_str(&s).map_err(reject))
            .transpose()
    }

    async fn get_config_revision_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        let p = NS;
        let row: Option<(String, i64, i64, i64)> = sqlx::query_as(&format!(
            "SELECT a.data, a.generation, \
             CAST(strftime('%s', first.created_at) AS INTEGER) * 1000, \
             CAST(strftime('%s', current.created_at) AS INTEGER) * 1000 \
             FROM {p}_agent a \
             JOIN {p}_agent_revision first ON first.scope_id = a.scope_id \
               AND first.id = a.id AND first.generation = 1 \
             JOIN {p}_agent_revision current ON current.scope_id = a.scope_id \
               AND current.id = a.id AND current.generation = a.generation \
             WHERE a.id = ?1 AND a.scope_id = ?2"
        ))
        .bind(id)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        row.map(|(data, revision, created_at_unix_ms, updated_at_unix_ms)| {
            Ok(AgentConfigRevision {
                config: serde_json::from_str(&data).map_err(reject)?,
                revision: unsigned(revision)?,
                created_at_unix_ms: Some(unsigned(created_at_unix_ms)?),
                updated_at_unix_ms: Some(unsigned(updated_at_unix_ms)?),
            })
        })
        .transpose()
    }

    async fn list_config_revisions_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        let p = NS;
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(&format!(
            "SELECT data, generation, \
             CAST(strftime('%s', (SELECT MIN(first.created_at) \
               FROM {p}_agent_revision first WHERE first.scope_id = ?1 AND first.id = ?2)) \
               AS INTEGER) * 1000, \
             CAST(strftime('%s', created_at) AS INTEGER) * 1000 \
             FROM {p}_agent_revision \
             WHERE scope_id = ?1 AND id = ?2 ORDER BY generation ASC"
        ))
        .bind(&scope.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|(data, revision, created_at_unix_ms, updated_at_unix_ms)| {
                Ok(AgentConfigRevision {
                    config: serde_json::from_str(&data).map_err(reject)?,
                    revision: unsigned(revision)?,
                    created_at_unix_ms: Some(unsigned(created_at_unix_ms)?),
                    updated_at_unix_ms: Some(unsigned(updated_at_unix_ms)?),
                })
            })
            .collect()
    }

    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        let p = NS;
        let rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_agent WHERE scope_id = ?1 ORDER BY id ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.iter()
            .map(|data| serde_json::from_str(data).map_err(reject))
            .collect()
    }

    async fn put_publication_if_source_current_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
        let p = NS;
        let fingerprint = publication.fingerprint.as_str();
        let agent_id = publication.agent_id.as_str();
        let state = publication.state.as_str();
        let record = serde_json::to_string(publication).map_err(reject)?;
        let source_revision = publication.source_revision;
        let execution_workspace = &publication.execution_workspace;
        let scope = scope.0.as_str();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current_revision = current_generation(&mut tx, p, scope, agent_id).await?;
        if let crate::PublicationSourceAdmission::Conflict { current_revision } =
            crate::PublicationSourceAdmission::new(current_revision, source_revision)
        {
            return Ok(PublicationWrite::Conflict { current_revision });
        }
        let existing = sqlx::query_scalar::<_, String>(&format!(
            "SELECT record FROM {p}_publication \
             WHERE scope_id = ?1 AND agent_id = ?2"
        ))
        .bind(scope)
        .bind(agent_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?
        .iter()
        .map(|record| decode_publication(record, &ScopeId::from(scope)).map_err(reject))
        .collect::<Result<Vec<_>, _>>()?;
        let decision = publication_revision_decision(
            execution_workspace.as_str(),
            source_revision,
            fingerprint,
            existing.iter().map(|existing| {
                (
                    existing.execution_workspace.as_str(),
                    existing.source_revision,
                    existing.fingerprint.as_str(),
                )
            }),
        );
        if decision == PublicationRevisionDecision::Conflict {
            return Ok(PublicationWrite::Conflict { current_revision });
        }
        insert_publication(&mut tx, p, fingerprint, agent_id, state, &record, scope).await?;
        tx.commit().await.map_err(reject)?;
        Ok(PublicationWrite::Committed { source_revision })
    }

    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let p = NS;
        let row: Option<(String, i64, i64)> = sqlx::query_as(&format!(
            "SELECT record, rowid, CAST(strftime('%s', created_at) AS INTEGER) * 1000 \
             FROM {p}_publication WHERE fingerprint = ?1 AND scope_id = ?2"
        ))
        .bind(fingerprint)
        .bind(&scope.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        row.map(|(record, sequence, published_at)| {
            let publication = decode_publication(&record, scope).map_err(reject)?;
            attach_publication_row(publication, sequence, published_at, true)
        })
        .transpose()
    }

    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
        let p = NS;
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(&format!(
            "SELECT record, rowid, CAST(strftime('%s', created_at) AS INTEGER) * 1000 \
             FROM {p}_publication \
             WHERE scope_id = ?1 AND state = 'published' ORDER BY rowid ASC"
        ))
        .bind(&scope.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|(record, sequence, published_at)| {
                let publication = decode_publication(&record, scope).map_err(reject)?;
                attach_publication_row(publication, sequence, published_at, true)
            })
            .collect()
    }
}

/// The scope-free [`ConfigRegistry`] over SQLite operates in the seeded
/// [`DEFAULT_SCOPE`] — byte-identical to the pre-tenancy behavior (one owner), so
/// existing single-machine callers are unchanged. Multi-tenant callers wrap with
/// [`crate::ScopedConfig`] bound to the request's scope.
#[async_trait]
impl ConfigRegistry for SqliteConfigStore {
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

#[cfg(test)]
mod scope_tests {
    use super::*;
    use awaken_agent_config::ScopedConfigRegistry;
    use awaken_agent_contract::ScopeId;

    fn agent(id: &str) -> AgentConfig {
        // A valid authoring aggregate; only the id matters for isolation.
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: Some(8),
            delegation_limits: Default::default(),
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b")
                .expect("literal coordinates"),
            tool_ids: Vec::new(),
            model_fallbacks: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
            tool_patterns: Vec::new(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn durable_audit_intent_and_config_commit_are_atomic_and_replay_safe() {
        let store = SqliteConfigStore::open_in_memory().await.unwrap();
        let scope = ScopeId::from("ws_a");
        let audit = ManagementAuditRecord {
            tool: "admin_draft_agent".into(),
            call_id: "call_1".into(),
            summary: "draft agent `a`".into(),
        };

        assert_eq!(
            store
                .record_management_audit_scoped(&scope, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        let original = agent("a");
        assert_eq!(
            store
                .put_config_with_audit_scoped(&scope, &original, 0, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        let generation = store
            .get_config_revision_scoped(&scope, "a")
            .await
            .unwrap()
            .unwrap()
            .revision;

        let mut conflicting_retry = original.clone();
        conflicting_retry.instructions = "must not overwrite on replay".into();
        assert_eq!(
            store
                .put_config_with_audit_scoped(&scope, &conflicting_retry, generation, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Replayed
        );
        let after = store
            .get_config_revision_scoped(&scope, "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.revision, generation);
        assert_eq!(after.config.instructions, original.instructions);

        let conflicting_audit = ManagementAuditRecord {
            summary: "different operation".into(),
            ..audit
        };
        assert!(
            store
                .record_management_audit_scoped(&scope, &conflicting_audit)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn audited_same_agent_id_is_committed_independently_in_each_scope() {
        let store = SqliteConfigStore::open_in_memory().await.unwrap();
        let owner = ScopeId::from("ws_owner");
        let attacker = ScopeId::from("ws_attacker");
        store
            .put_config_scoped(&owner, &agent("shared"))
            .await
            .unwrap();
        let audit = ManagementAuditRecord {
            tool: "admin_patch_agent".into(),
            call_id: "call_cross_scope".into(),
            summary: "patch agent `shared`".into(),
        };
        store
            .record_management_audit_scoped(&attacker, &audit)
            .await
            .unwrap();

        let mut attempted = agent("shared");
        attempted.instructions = "cross-scope overwrite".into();
        assert_eq!(
            store
                .put_config_with_audit_scoped(&attacker, &attempted, 0, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        assert_eq!(
            store
                .get_config_scoped(&attacker, "shared")
                .await
                .unwrap()
                .unwrap()
                .instructions,
            "cross-scope overwrite"
        );
        assert_eq!(
            store
                .get_config_scoped(&owner, "shared")
                .await
                .unwrap()
                .unwrap()
                .instructions,
            "be helpful"
        );
    }

    #[tokio::test]
    async fn a_scope_reads_back_its_own_agent() {
        let store = SqliteConfigStore::open_in_memory().await.expect("open");
        let a = ScopeId::from("ws_a");
        store.put_config_scoped(&a, &agent("x")).await.expect("put");
        assert_eq!(
            store
                .get_config_scoped(&a, "x")
                .await
                .expect("get")
                .map(|c| c.id),
            Some("x".to_string())
        );
    }

    #[tokio::test]
    async fn another_scope_cannot_read_the_agent() {
        let store = SqliteConfigStore::open_in_memory().await.expect("open");
        store
            .put_config_scoped(&ScopeId::from("ws_a"), &agent("x"))
            .await
            .expect("put");
        // Same id, different scope → invisible (the isolation fence).
        assert!(
            store
                .get_config_scoped(&ScopeId::from("ws_b"), "x")
                .await
                .expect("get")
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_same_agent_id_is_stored_independently_in_each_scope() {
        let store = SqliteConfigStore::open_in_memory().await.expect("open");
        let a = ScopeId::from("ws_a");
        let b = ScopeId::from("ws_b");
        store
            .put_config_scoped(&a, &agent("x"))
            .await
            .expect("put a");
        let mut b_agent = agent("x");
        b_agent.instructions = "B-data".into();
        store.put_config_scoped(&b, &b_agent).await.expect("put b");
        assert_eq!(
            store
                .get_config_scoped(&a, "x")
                .await
                .expect("get a")
                .unwrap()
                .instructions,
            "be helpful"
        );
        assert_eq!(
            store
                .get_config_scoped(&b, "x")
                .await
                .expect("get b")
                .unwrap()
                .instructions,
            "B-data"
        );
    }

    #[tokio::test]
    async fn the_scope_free_port_uses_the_default_scope() {
        let store = SqliteConfigStore::open_in_memory().await.expect("open");
        // A scope-free write lands under DEFAULT_SCOPE and is readable scope-free…
        store.put_config(&agent("d")).await.expect("put");
        assert!(store.get_config("d").await.expect("get").is_some());
        // …and via the explicit default scope, but not another scope.
        assert!(
            store
                .get_config_scoped(&ScopeId::from(DEFAULT_SCOPE), "d")
                .await
                .expect("get")
                .is_some()
        );
        assert!(
            store
                .get_config_scoped(&ScopeId::from("ws_other"), "d")
                .await
                .expect("get")
                .is_none()
        );
    }
}
