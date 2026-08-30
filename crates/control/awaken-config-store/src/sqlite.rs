//! SQLite config-store adapter under the built-in `config` namespace.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};

use awaken_tenancy::ScopeId;

use crate::codec::decode_publication;
use crate::schema::{BUNDLE_ID, converged_config_bundle, selected_config_bundle};
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError,
    ConfigWrite, DEFAULT_SCOPE, ManagementAuditEntry, ManagementAuditRecord, ManagementEffect,
    PublicationRevisionDecision, ScopedConfigRegistry, StoredPublication,
    publication_revision_decision,
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
    conn: awaken_sqlite_runtime::SharedSqliteConnection,
}

impl SqliteConfigStore {
    /// Open (or create) a database file and apply the config migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
            .open()
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::memory()
            .open()
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let ledger_exists = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [format!("{NS}_schema_migrations")],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let v1_checksum = if ledger_exists {
            conn.query_row(
                &format!(
                    "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
                ),
                [BUNDLE_ID],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| StoreError::Migrate(error.to_string()))?
        } else {
            None
        };
        let published = selected_config_bundle(v1_checksum.as_deref())
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let converged =
            converged_config_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
        let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        runner
            .run_bundle(&conn, &published)
            .and_then(|_| runner.run_bundle(&conn, &converged))
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        Ok(Self {
            conn: awaken_sqlite_runtime::SharedSqliteConnection::new(conn),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, ConfigStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, ConfigStoreError> + Send + 'static,
    {
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| f(conn, NS))
            .await
            .map_err(|err| ConfigStoreError(err.to_string()))?
    }
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

#[async_trait]
impl ScopedConfigRegistry for SqliteConfigStore {
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError> {
        self.with_conn(move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT DISTINCT scope_id FROM {p}_agent ORDER BY scope_id ASC"
                ))
                .map_err(reject)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(reject)?;
            let mut scopes = Vec::new();
            for row in rows {
                scopes.push(ScopeId::from(row.map_err(reject)?));
            }
            Ok(scopes)
        })
        .await
    }

    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            // Agent identity is `(scope_id, id)`: another scope may reuse the
            // same portable Agent id without reading or clobbering this row.
            conn.execute(
                &format!(
                    "INSERT INTO {p}_agent (id, data, scope_id, generation) VALUES (?1, ?2, ?3, 1) \
                     ON CONFLICT(scope_id, id) DO UPDATE SET data = excluded.data, \
                     generation = {p}_agent.generation + 1"
                ),
                params![id, data, scope],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
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
        let config = config.clone();
        let id = config.id.clone();
        let scope = scope.0.clone();
        let call_id = format!("{}:{}", audit.tool, audit.call_id);
        let audit_data = serde_json::to_string(audit).map_err(reject)?;
        let effect = effect.cloned();
        self.with_conn(move |conn, p| {
            let tx = conn.transaction().map_err(reject)?;
            let existing: Option<(String, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT record, business_committed FROM {p}_management_audit \
                         WHERE scope_id = ?1 AND call_id = ?2"
                    ),
                    params![scope, call_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            let replayed = if let Some((existing, committed)) = existing {
                if existing != audit_data {
                    return Err(ConfigStoreError(
                        "stable audit call id was reused with different content".into(),
                    ));
                }
                committed != 0
            } else {
                return Err(ConfigStoreError(
                    "audited config transaction requires a pre-recorded management audit".into(),
                ));
            };
            if replayed {
                tx.commit().map_err(reject)?;
                return Ok(AuditedConfigWrite::Replayed);
            }
            let current: Option<(String, u64)> = tx
                .query_row(
                    &format!(
                        "SELECT data, generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
                    ),
                    params![id, scope],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            let current_revision = current.as_ref().map(|(_, revision)| *revision);
            if current_revision.unwrap_or(0) != expected_generation {
                return Ok(AuditedConfigWrite::Conflict { current_revision });
            }
            let current_config = current
                .as_ref()
                .map(|(data, _)| serde_json::from_str::<AgentConfig>(data).map_err(reject))
                .transpose()?;
            let config = config
                .canonicalize_mutable_authoring_against(current_config.as_ref())
                .map_err(reject)?;
            let data = serde_json::to_string(&config).map_err(reject)?;
            if let Some(effect) = effect {
                let effect_payload = serde_json::to_string(&effect).map_err(reject)?;
                let existing_payload: Option<String> = tx
                    .query_row(
                        &format!(
                            "SELECT payload FROM {p}_management_effect \
                             WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
                        ),
                        params![scope, effect.kind(), effect.key()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(reject)?;
                match existing_payload {
                    Some(payload) if payload != effect_payload => {
                        return Err(ConfigStoreError(
                            "stable management effect key was reused with different content".into(),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        tx.execute(
                            &format!(
                                "INSERT INTO {p}_management_effect \
                                 (scope_id, kind, effect_key, payload) VALUES (?1, ?2, ?3, ?4)"
                            ),
                            params![scope, effect.kind(), effect.key(), effect_payload],
                        )
                        .map_err(reject)?;
                    }
                }
            }
            let changed = tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_agent (id, data, scope_id, generation) \
                         VALUES (?1, ?2, ?3, 1) ON CONFLICT(scope_id, id) DO UPDATE SET \
                         data = excluded.data, generation = {p}_agent.generation + 1 \
                         WHERE {p}_agent.generation = ?4"
                    ),
                    params![id, data, scope, expected_generation],
                )
                .map_err(reject)?;
            if changed != 1 {
                let current_revision = tx
                    .query_row(
                        &format!(
                            "SELECT generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
                        ),
                        params![id, scope],
                        |row| row.get::<_, u64>(0),
                    )
                    .optional()
                    .map_err(reject)?;
                return Ok(AuditedConfigWrite::Conflict { current_revision });
            }
            tx.execute(
                &format!(
                    "UPDATE {p}_management_audit SET business_committed = 1 \
                     WHERE scope_id = ?1 AND call_id = ?2"
                ),
                params![scope, call_id],
            )
            .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(AuditedConfigWrite::Applied)
        })
        .await
    }

    async fn pending_management_effects_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<ManagementEffect>, ConfigStoreError> {
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT kind, effect_key, payload FROM {p}_management_effect \
                     WHERE scope_id = ?1 ORDER BY created_at, kind, effect_key"
                ))
                .map_err(reject)?;
            let rows = statement
                .query_map(params![scope], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(reject)?;
            rows.map(|row| {
                let (kind, key, payload) = row.map_err(reject)?;
                let effect: ManagementEffect = serde_json::from_str(&payload).map_err(reject)?;
                if kind != effect.kind() || key != effect.key() {
                    return Err(ConfigStoreError(
                        "management effect index does not match its typed payload".into(),
                    ));
                }
                Ok(effect)
            })
            .collect()
        })
        .await
    }

    async fn complete_management_effect_scoped(
        &self,
        scope: &ScopeId,
        kind: &str,
        key: &str,
    ) -> Result<(), ConfigStoreError> {
        let scope = scope.0.clone();
        let kind = kind.to_string();
        let key = key.to_string();
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "DELETE FROM {p}_management_effect \
                     WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
                ),
                params![scope, kind, key],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn record_management_audit_scoped(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let scope = scope.0.clone();
        let call_id = format!("{}:{}", audit.tool, audit.call_id);
        let data = serde_json::to_string(audit).map_err(reject)?;
        self.with_conn(move |conn, p| {
            let existing: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT record FROM {p}_management_audit WHERE scope_id = ?1 AND call_id = ?2"
                    ),
                    params![scope, call_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(reject)?;
            if let Some(existing) = existing {
                return if existing == data {
                    Ok(AuditedConfigWrite::Replayed)
                } else {
                    Err(ConfigStoreError(
                        "stable audit call id was reused with different content".into(),
                    ))
                };
            }
            conn.execute(
                &format!(
                    "INSERT INTO {p}_management_audit (scope_id, call_id, record, business_committed) VALUES (?1, ?2, ?3, 0)"
                ),
                params![scope, call_id, data],
            )
            .map_err(reject)?;
            Ok(AuditedConfigWrite::Applied)
        })
        .await
    }

    async fn get_management_audit_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        let scope = scope.0.clone();
        let audit_key = format!("{tool}:{call_id}");
        self.with_conn(move |conn, p| {
            let row: Option<(String, i64)> = conn
                .query_row(
                    &format!(
                        "SELECT record, business_committed FROM {p}_management_audit \
                         WHERE scope_id = ?1 AND call_id = ?2"
                    ),
                    params![scope, audit_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            row.map(|(record, business_committed)| {
                Ok(ManagementAuditEntry {
                    record: serde_json::from_str(&record).map_err(reject)?,
                    business_committed: business_committed != 0,
                })
            })
            .transpose()
        })
        .await
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        let scope = scope.0.clone();
        let audit_key = format!("{tool}:{call_id}");
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "UPDATE {p}_management_audit SET business_committed = 1 \
                         WHERE scope_id = ?1 AND call_id = ?2"
                    ),
                    params![scope, audit_key],
                )
                .map_err(reject)?;
            if changed != 1 {
                return Err(ConfigStoreError(
                    "management audit completion target was not found".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    async fn put_config_if_revision_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_agent (id, data, scope_id, generation) \
                         VALUES (?1, ?2, ?3, 1) ON CONFLICT(scope_id, id) DO UPDATE SET \
                         data = excluded.data, generation = {p}_agent.generation + 1 \
                         WHERE {p}_agent.generation = ?4"
                    ),
                    params![id, data, scope, expected_generation],
                )
                .map_err(reject)?;
            if changed == 1 {
                return Ok(ConfigWrite::Applied {
                    revision: expected_generation.saturating_add(1).max(1),
                });
            }
            let current_revision = conn
                .query_row(
                    &format!("SELECT generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"),
                    params![id, scope],
                    |row| row.get::<_, u64>(0),
                )
                .optional()
                .map_err(reject)?;
            Ok(ConfigWrite::Conflict { current_revision })
        })
        .await
    }

    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError> {
        let id = id.to_string();
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"),
                    params![id, scope],
                    |r| r.get(0),
                )
                .optional()
                .map_err(reject)?;
            data.map(|s| serde_json::from_str(&s).map_err(reject))
                .transpose()
        })
        .await
    }

    async fn get_config_revision_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        let id = id.to_string();
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let row: Option<(String, u64, u64, u64)> = conn
                .query_row(
                    &format!(
                        "SELECT a.data, a.generation, \
                         CAST(strftime('%s', first.created_at) AS INTEGER) * 1000, \
                         CAST(strftime('%s', current.created_at) AS INTEGER) * 1000 \
                         FROM {p}_agent a \
                         JOIN {p}_agent_revision first ON first.scope_id = a.scope_id \
                           AND first.id = a.id AND first.generation = 1 \
                         JOIN {p}_agent_revision current ON current.scope_id = a.scope_id \
                           AND current.id = a.id AND current.generation = a.generation \
                         WHERE a.id = ?1 AND a.scope_id = ?2"
                    ),
                    params![id, scope],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(reject)?;
            row.map(|(data, revision, created_at_unix_ms, updated_at_unix_ms)| {
                Ok(AgentConfigRevision {
                    config: serde_json::from_str(&data).map_err(reject)?,
                    revision,
                    created_at_unix_ms: Some(created_at_unix_ms),
                    updated_at_unix_ms: Some(updated_at_unix_ms),
                })
            })
            .transpose()
        })
        .await
    }

    async fn list_config_revisions_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        let id = id.to_string();
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT data, generation, \
                     CAST(strftime('%s', (SELECT MIN(first.created_at) \
                       FROM {p}_agent_revision first WHERE first.scope_id = ?1 AND first.id = ?2)) \
                       AS INTEGER) * 1000, \
                     CAST(strftime('%s', created_at) AS INTEGER) * 1000 \
                     FROM {p}_agent_revision \
                     WHERE scope_id = ?1 AND id = ?2 ORDER BY generation ASC"
                ))
                .map_err(reject)?;
            let rows = statement
                .query_map(params![scope, id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                })
                .map_err(reject)?;
            let mut revisions = Vec::new();
            for row in rows {
                let (data, revision, created_at_unix_ms, updated_at_unix_ms) =
                    row.map_err(reject)?;
                revisions.push(AgentConfigRevision {
                    config: serde_json::from_str(&data).map_err(reject)?,
                    revision,
                    created_at_unix_ms: Some(created_at_unix_ms),
                    updated_at_unix_ms: Some(updated_at_unix_ms),
                });
            }
            Ok(revisions)
        })
        .await
    }

    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT data FROM {p}_agent WHERE scope_id = ?1 ORDER BY id ASC"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map(params![scope], |r| r.get::<_, String>(0))
                .map_err(reject)?;
            let mut configs = Vec::new();
            for row in rows {
                let data = row.map_err(reject)?;
                configs.push(serde_json::from_str(&data).map_err(reject)?);
            }
            Ok(configs)
        })
        .await
    }

    async fn put_publication_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        let fingerprint = publication.fingerprint.clone();
        let agent_id = publication.agent_id.clone();
        let state = publication.state.as_str().to_string();
        let record = serde_json::to_string(publication).map_err(reject)?;
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_publication (fingerprint, agent_id, state, record, scope_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(scope_id, fingerprint) DO NOTHING"
                ),
                params![fingerprint, agent_id, state, record, scope],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn put_publication_if_config_revision_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let fingerprint = publication.fingerprint.clone();
        let agent_id = publication.agent_id.clone();
        let state = publication.state.as_str().to_string();
        let record = serde_json::to_string(publication).map_err(reject)?;
        let source_revision = publication.source_revision;
        let execution_workspace = publication.execution_workspace.clone();
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let tx = conn.transaction().map_err(reject)?;
            let current_revision = tx
                .query_row(
                    &format!("SELECT generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"),
                    params![agent_id, scope],
                    |row| row.get::<_, u64>(0),
                )
                .optional()
                .map_err(reject)?;
            if current_revision != Some(expected_generation) {
                return Ok(ConfigWrite::Conflict { current_revision });
            }
            let existing = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT record FROM {p}_publication \
                         WHERE scope_id = ?1 AND agent_id = ?2"
                    ))
                    .map_err(reject)?;
                let rows = statement
                    .query_map(params![scope, agent_id], |row| row.get::<_, String>(0))
                    .map_err(reject)?;
                let mut existing = Vec::new();
                for row in rows {
                    existing.push(
                        decode_publication(&row.map_err(reject)?, &ScopeId::from(scope.as_str()))
                            .map_err(reject)?,
                    );
                }
                existing
            };
            let decision = publication_revision_decision(
                execution_workspace.as_str(),
                source_revision,
                fingerprint.as_str(),
                existing.iter().map(|existing| {
                    (
                        existing.execution_workspace.as_str(),
                        existing.source_revision,
                        existing.fingerprint.as_str(),
                    )
                }),
            );
            if decision == PublicationRevisionDecision::Conflict {
                return Ok(ConfigWrite::Conflict { current_revision });
            }
            tx.execute(
                &format!(
                    "INSERT INTO {p}_publication (fingerprint, agent_id, state, record, scope_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(scope_id, fingerprint) DO NOTHING"
                ),
                params![fingerprint, agent_id, state, record, scope],
            )
            .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(ConfigWrite::Applied {
                revision: expected_generation,
            })
        })
        .await
    }

    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let fingerprint = fingerprint.to_string();
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let record: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT record FROM {p}_publication WHERE fingerprint = ?1 AND scope_id = ?2"
                    ),
                    params![fingerprint, scope],
                    |r| r.get(0),
                )
                .optional()
                .map_err(reject)?;
            record
                .map(|record| decode_publication(&record, &ScopeId::from(scope.as_str())))
                .transpose()
                .map_err(reject)
        })
        .await
    }

    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT record FROM {p}_publication \
                     WHERE scope_id = ?1 AND state = 'published' ORDER BY rowid ASC"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map(params![scope], |r| r.get::<_, String>(0))
                .map_err(reject)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(
                    decode_publication(&row.map_err(reject)?, &ScopeId::from(scope.as_str()))
                        .map_err(reject)?,
                );
            }
            Ok(out)
        })
        .await
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

#[cfg(test)]
mod scope_tests {
    use super::*;
    use awaken_agent_config::ScopedConfigRegistry;
    use awaken_tenancy::ScopeId;

    fn agent(id: &str) -> AgentConfig {
        // A valid authoring aggregate; only the id matters for isolation.
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
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
        let store = SqliteConfigStore::open_in_memory().unwrap();
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
        let store = SqliteConfigStore::open_in_memory().unwrap();
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
        let store = SqliteConfigStore::open_in_memory().expect("open");
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
        let store = SqliteConfigStore::open_in_memory().expect("open");
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
        let store = SqliteConfigStore::open_in_memory().expect("open");
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
        let store = SqliteConfigStore::open_in_memory().expect("open");
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
