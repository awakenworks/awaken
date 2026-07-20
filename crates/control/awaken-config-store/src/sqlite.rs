//! SQLite config-store adapter under the built-in `config` namespace.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};

use awaken_tenancy::ScopeId;

use crate::config::AgentConfig;
use crate::schema::config_bundle;
use crate::store::{
    AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError, ConfigWrite,
    DEFAULT_SCOPE, ManagementAuditEntry, ManagementAuditRecord, ManagementEffect,
    ScopedConfigRegistry, StoredPublication,
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
    conn: Arc<Mutex<Connection>>,
}

impl SqliteConfigStore {
    /// Open (or create) a database file and apply the config migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let bundle = config_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, ConfigStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, ConfigStoreError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| ConfigStoreError("config connection poisoned".to_string()))?;
            f(&mut guard, NS)
        })
        .await
        .map_err(|err| ConfigStoreError(err.to_string()))?
    }
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

#[async_trait]
impl ScopedConfigRegistry for SqliteConfigStore {
    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.clone();
        self.with_conn(move |conn, p| {
            // The `ON CONFLICT … WHERE` guard makes a cross-scope write a no-op:
            // if the existing row belongs to another scope, the update is skipped
            // (a workspace cannot clobber another's agent by id), and a same-scope
            // write updates the data.
            conn.execute(
                &format!(
                    "INSERT INTO {p}_agent (id, data, scope_id, generation) VALUES (?1, ?2, ?3, 1) \
                     ON CONFLICT(id) DO UPDATE SET data = excluded.data, \
                     generation = {p}_agent.generation + 1 \
                     WHERE {p}_agent.scope_id = excluded.scope_id"
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
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
        let scope = scope.0.clone();
        let call_id = format!("{}:{}", audit.tool, audit.call_id);
        let audit_data = serde_json::to_string(audit).map_err(reject)?;
        self.with_conn(move |conn, p| {
            let tx = conn.transaction().map_err(reject)?;
            let existing: Option<(String, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT record, business_committed FROM {p}_management_audit WHERE scope_id = ?1 AND call_id = ?2"
                    ),
                    params![scope, call_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            if let Some((existing, committed)) = existing {
                if existing != audit_data {
                    return Err(ConfigStoreError(
                        "stable audit call id was reused with different content".into(),
                    ));
                }
                if committed != 0 {
                    return Ok(AuditedConfigWrite::Replayed);
                }
            } else {
                tx.execute(
                    &format!(
                        "INSERT INTO {p}_management_audit (scope_id, call_id, record) VALUES (?1, ?2, ?3)"
                    ),
                    params![scope, call_id, audit_data],
                )
                .map_err(reject)?;
            }
            let changed = tx
                .execute(
                &format!(
                    "INSERT INTO {p}_agent (id, data, scope_id, generation) VALUES (?1, ?2, ?3, 1) \
                     ON CONFLICT(id) DO UPDATE SET data = excluded.data, \
                     generation = {p}_agent.generation + 1 \
                     WHERE {p}_agent.scope_id = excluded.scope_id"
                ),
                params![id, data, scope],
            )
                .map_err(reject)?;
            if changed != 1 {
                return Err(ConfigStoreError(
                    "audited config write was fenced by another scope".into(),
                ));
            }
            tx.execute(
                &format!(
                    "UPDATE {p}_management_audit SET business_committed = 1 WHERE scope_id = ?1 AND call_id = ?2"
                ),
                params![scope, call_id],
            )
            .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(AuditedConfigWrite::Applied)
        })
        .await
    }

    async fn put_config_with_audit_effect_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
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
                tx.execute(
                    &format!(
                        "INSERT INTO {p}_management_audit (scope_id, call_id, record) \
                         VALUES (?1, ?2, ?3)"
                    ),
                    params![scope, call_id, audit_data],
                )
                .map_err(reject)?;
                false
            };
            if let Some(effect) = effect {
                let effect_payload = serde_json::to_string(&effect.payload).map_err(reject)?;
                let existing_payload: Option<String> = tx
                    .query_row(
                        &format!(
                            "SELECT payload FROM {p}_management_effect \
                             WHERE scope_id = ?1 AND kind = ?2 AND effect_key = ?3"
                        ),
                        params![scope, effect.kind, effect.key],
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
                            params![scope, effect.kind, effect.key, effect_payload],
                        )
                        .map_err(reject)?;
                    }
                }
            }
            if replayed {
                tx.commit().map_err(reject)?;
                return Ok(AuditedConfigWrite::Replayed);
            }
            let changed = tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_agent (id, data, scope_id, generation) \
                         VALUES (?1, ?2, ?3, 1) ON CONFLICT(id) DO UPDATE SET \
                         data = excluded.data, generation = {p}_agent.generation + 1 \
                         WHERE {p}_agent.scope_id = excluded.scope_id"
                    ),
                    params![id, data, scope],
                )
                .map_err(reject)?;
            if changed != 1 {
                return Err(ConfigStoreError(
                    "audited config write was fenced by another scope".into(),
                ));
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
                Ok(ManagementEffect {
                    kind,
                    key,
                    payload: serde_json::from_str(&payload).map_err(reject)?,
                })
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
                         VALUES (?1, ?2, ?3, 1) ON CONFLICT(id) DO UPDATE SET \
                         data = excluded.data, generation = {p}_agent.generation + 1 \
                         WHERE {p}_agent.scope_id = excluded.scope_id \
                         AND {p}_agent.generation = ?4"
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
            let row: Option<(String, u64)> = conn
                .query_row(
                    &format!(
                        "SELECT data, generation FROM {p}_agent WHERE id = ?1 AND scope_id = ?2"
                    ),
                    params![id, scope],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            row.map(|(data, revision)| {
                Ok(AgentConfigRevision {
                    config: serde_json::from_str(&data).map_err(reject)?,
                    revision,
                })
            })
            .transpose()
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
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(fingerprint) DO NOTHING"
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
            tx.execute(
                &format!(
                    "INSERT INTO {p}_publication (fingerprint, agent_id, state, record, scope_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(fingerprint) DO NOTHING"
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
                .map(|s| serde_json::from_str(&s).map_err(reject))
                .transpose()
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
                out.push(serde_json::from_str(&row.map_err(reject)?).map_err(reject)?);
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
    use crate::store::ScopedConfigRegistry;
    use awaken_tenancy::ScopeId;

    fn agent(id: &str) -> AgentConfig {
        // A valid authoring aggregate; only the id matters for isolation.
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: crate::config::ModelSelection::pinned("p", "m", "b"),
            tool_ids: Vec::new(),
            model_candidates: Vec::new(),
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
                .put_config_with_audit_scoped(&scope, &original, &audit)
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
                .put_config_with_audit_scoped(&scope, &conflicting_retry, &audit)
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
    async fn a_scope_fence_rolls_back_the_audit_commit_too() {
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
        assert!(
            store
                .put_config_with_audit_scoped(&attacker, &attempted, &audit)
                .await
                .is_err()
        );
        assert!(
            store
                .get_config_scoped(&attacker, "shared")
                .await
                .unwrap()
                .is_none()
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
    async fn a_write_cannot_clobber_another_scopes_agent() {
        let store = SqliteConfigStore::open_in_memory().expect("open");
        let a = ScopeId::from("ws_a");
        let b = ScopeId::from("ws_b");
        store
            .put_config_scoped(&a, &agent("x"))
            .await
            .expect("put a");
        // ws_b attempts to overwrite id "x" — the conflict guard makes it a no-op.
        store
            .put_config_scoped(&b, &agent("x"))
            .await
            .expect("put b");
        // ws_a still owns "x"; ws_b still cannot see it.
        assert!(
            store
                .get_config_scoped(&a, "x")
                .await
                .expect("get a")
                .is_some()
        );
        assert!(
            store
                .get_config_scoped(&b, "x")
                .await
                .expect("get b")
                .is_none()
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
