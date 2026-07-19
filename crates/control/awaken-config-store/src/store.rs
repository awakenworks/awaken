//! The `ConfigRegistry` port and its persisted aggregates.

use std::sync::Arc;

use awaken_runtime_contract::catalog::RuntimeCatalogInstall;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_tenancy::ScopeId;
use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

/// A neutral config-store failure (storage or serialization). Compilation errors
/// are separate ([`crate::CompileError`]).
#[derive(Debug, thiserror::Error)]
#[error("config store: {0}")]
pub struct ConfigStoreError(pub String);

/// An authoring config paired with the monotonic generation used for optimistic
/// concurrency control.
#[derive(Debug, Clone)]
pub struct VersionedAgentConfig {
    pub config: AgentConfig,
    pub generation: u64,
}

/// Outcome of an atomic compare-and-set config write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWrite {
    Applied { generation: u64 },
    Conflict { current_generation: Option<u64> },
}

/// Secret-free durable management audit record keyed by stable tool call id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementAuditRecord {
    pub tool: String,
    pub call_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementAuditEntry {
    pub record: ManagementAuditRecord,
    pub business_committed: bool,
}

/// A secret-free, idempotent effect that must be applied to a separate store
/// after the config transaction commits. The config store durably journals it
/// in the same transaction as the draft and audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementEffect {
    pub kind: String,
    pub key: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditedConfigWrite {
    Applied,
    Replayed,
}

/// The lifecycle spine (ADR-0031). The richer states (installing/active/
/// superseded/rolled_back/rejected) are deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicationState {
    Compiled,
    Published,
}

impl PublicationState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PublicationState::Compiled => "compiled",
            PublicationState::Published => "published",
        }
    }
}

/// A persisted publication: the compiled artifact plus its lifecycle state. It is
/// content-addressed by `fingerprint`, so storing it again is idempotent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPublication {
    pub publication_id: String,
    pub fingerprint: String,
    pub agent_id: String,
    #[serde(default)]
    pub source_generation: u64,
    pub state: PublicationState,
    pub snapshot: ExecutableAgentSnapshot,
    pub install: RuntimeCatalogInstall,
}

impl StoredPublication {
    /// Wrap a freshly compiled config as `published`. The fingerprint and
    /// publication id come from the runnable config itself (the producer stamped
    /// them), so the store never re-derives them.
    pub fn published(config: RunnableConfig, agent_id: impl Into<String>) -> Self {
        Self::published_at_generation(config, agent_id, 0)
    }

    pub fn published_at_generation(
        config: RunnableConfig,
        agent_id: impl Into<String>,
        source_generation: u64,
    ) -> Self {
        let (snapshot, install) = config.into_parts();
        Self {
            publication_id: install.publication_id.clone(),
            fingerprint: snapshot.fingerprint.0.clone(),
            agent_id: agent_id.into(),
            source_generation,
            state: PublicationState::Published,
            snapshot,
            install,
        }
    }
}

/// Durable storage for the config domain: agent configs (the authoring
/// aggregate) and publications (the compiled artifact). Adapters live under the
/// `config` table namespace, alongside the runtime's tables (ADR-0029).
#[async_trait::async_trait]
pub trait ConfigRegistry: Send + Sync {
    /// Upsert an agent config by id.
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError>;

    /// Store only when `expected_generation` is still current. Generation zero
    /// means "create only". Adapters without CAS support fail explicitly.
    async fn put_config_if_generation(
        &self,
        _config: &AgentConfig,
        _expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support generation CAS".to_string(),
        ))
    }

    /// Load an agent config by id.
    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError>;

    async fn get_config_versioned(
        &self,
        id: &str,
    ) -> Result<Option<VersionedAgentConfig>, ConfigStoreError> {
        Ok(self
            .get_config(id)
            .await?
            .map(|config| VersionedAgentConfig {
                config,
                generation: 0,
            }))
    }

    /// List every stored agent config (the authoring aggregate), ascending by id.
    /// Backs the management console's agent list, which authors against this
    /// config plane directly rather than the SDK-facing `/v1/agents` registry.
    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError>;

    /// Store a publication, idempotent by fingerprint.
    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

    async fn put_publication_if_config_generation(
        &self,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let current_generation = self
            .get_config_versioned(&publication.agent_id)
            .await?
            .map(|versioned| versioned.generation);
        if current_generation != Some(expected_generation) {
            return Ok(ConfigWrite::Conflict { current_generation });
        }
        self.put_publication(publication).await?;
        Ok(ConfigWrite::Applied {
            generation: expected_generation,
        })
    }

    /// Load a publication by its fingerprint.
    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError>;
}

// --- Tenant isolation: the ScopedConfig decorator (ADR-0051 D2/D4) ----------

/// The seeded owner scope every un-scoped write lands under and every un-scoped
/// read filters by. It matches the `scope_id` column default in the schema, so a
/// pre-tenancy row and a `DEFAULT_SCOPE` write are the same owner — the
/// single-machine "seeded, not absent" default (ADR-0048 D2).
pub const DEFAULT_SCOPE: &str = "default";

/// The scope-aware backing store — the infrastructure-facing half of the config
/// port. Each method carries an owner [`ScopeId`], persisted as one opaque
/// `scope_id` column: reads filter by it and writes are guarded by it, so a
/// workspace can neither read nor clobber another's agent by id. The core-facing
/// [`ConfigRegistry`] is scope-free; [`ScopedConfig`] bridges the two by binding a
/// scope.
#[async_trait::async_trait]
pub trait ScopedConfigRegistry: Send + Sync {
    /// Upsert an agent config owned by `scope` (a write never crosses into
    /// another scope's row of the same id).
    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError>;

    /// Atomically persist the audit record and config. Replaying the same call id
    /// with the same record is a no-op; conflicting reuse fails closed.
    async fn put_config_with_audit_scoped(
        &self,
        _scope: &ScopeId,
        _config: &AgentConfig,
        _audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support transactional audit".to_string(),
        ))
    }

    async fn put_config_with_audit_effect_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        if effect.is_some() {
            return Err(ConfigStoreError(
                "config registry does not support durable external effects".to_string(),
            ));
        }
        self.put_config_with_audit_scoped(scope, config, audit)
            .await
    }

    async fn pending_management_effects_scoped(
        &self,
        _scope: &ScopeId,
    ) -> Result<Vec<ManagementEffect>, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable external effects".to_string(),
        ))
    }

    async fn complete_management_effect_scoped(
        &self,
        _scope: &ScopeId,
        _kind: &str,
        _key: &str,
    ) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable external effects".to_string(),
        ))
    }

    async fn record_management_audit_scoped(
        &self,
        _scope: &ScopeId,
        _audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit".to_string(),
        ))
    }

    async fn get_management_audit_scoped(
        &self,
        _scope: &ScopeId,
        _tool: &str,
        _call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit reads".to_string(),
        ))
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        _scope: &ScopeId,
        _tool: &str,
        _call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit completion".to_string(),
        ))
    }

    async fn put_config_if_generation_scoped(
        &self,
        _scope: &ScopeId,
        _config: &AgentConfig,
        _expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "scoped config registry does not support generation CAS".to_string(),
        ))
    }

    /// Load an agent config by id **within `scope`** — a row owned by another
    /// scope is invisible.
    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError>;

    async fn get_config_versioned_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<VersionedAgentConfig>, ConfigStoreError> {
        Ok(self
            .get_config_scoped(scope, id)
            .await?
            .map(|config| VersionedAgentConfig {
                config,
                generation: 0,
            }))
    }

    /// List every agent config owned by `scope`, ascending by id — a row owned by
    /// another scope is invisible.
    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError>;

    /// Store a publication owned by `scope`, idempotent by fingerprint.
    async fn put_publication_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

    async fn put_publication_if_config_generation_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let current_generation = self
            .get_config_versioned_scoped(scope, &publication.agent_id)
            .await?
            .map(|versioned| versioned.generation);
        if current_generation != Some(expected_generation) {
            return Ok(ConfigWrite::Conflict { current_generation });
        }
        self.put_publication_scoped(scope, publication).await?;
        Ok(ConfigWrite::Applied {
            generation: expected_generation,
        })
    }

    /// Load a publication by fingerprint **within `scope`**.
    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError>;

    /// Every **published** publication owned by `scope`, oldest first (ascending by
    /// insertion order), so a warm-load that inserts into an agent-keyed map keeps
    /// the latest publication per agent.
    ///
    /// **Required** — deliberately has no default. It once defaulted to
    /// `Ok(Vec::new())` "because only a durable store reloads across a lifetime,"
    /// but that let a durable backend that simply *forgot* to override it compile
    /// clean and silently reload zero agents on restart (exactly what happened to
    /// the Postgres store). Forcing every implementor to answer means an empty
    /// reload is now an explicit choice, never an accident. An implementor with
    /// nothing to reload (a purely ephemeral store) returns `Ok(Vec::new())` on
    /// purpose.
    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError>;
}

/// The decorator that makes tenancy an edge aspect for the config plane: it
/// implements the scope-free [`ConfigRegistry`] by binding one [`ScopeId`] and
/// delegating to a [`ScopedConfigRegistry`]. Constructed at the management edge
/// from the request's resolved scope, so every authoring write auto-stamps the
/// bound owner and every read auto-filters by it — no call site can forget.
pub struct ScopedConfig<S: ScopedConfigRegistry + ?Sized> {
    inner: Arc<S>,
    scope: ScopeId,
}

impl<S: ScopedConfigRegistry + ?Sized> ScopedConfig<S> {
    /// Bind `store` to `scope` for one tenant's authoring requests. `S` may be a
    /// trait object (`dyn ScopedConfigRegistry`), so the edge can bind a boxed store.
    pub fn new(store: Arc<S>, scope: ScopeId) -> Self {
        Self {
            inner: store,
            scope,
        }
    }

    /// The scope this registry is bound to.
    #[must_use]
    pub fn scope(&self) -> &ScopeId {
        &self.scope
    }
}

#[async_trait::async_trait]
impl<S: ScopedConfigRegistry + ?Sized> ConfigRegistry for ScopedConfig<S> {
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError> {
        self.inner.put_config_scoped(&self.scope, config).await
    }

    async fn put_config_if_generation(
        &self,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.inner
            .put_config_if_generation_scoped(&self.scope, config, expected_generation)
            .await
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.inner.get_config_scoped(&self.scope, id).await
    }

    async fn get_config_versioned(
        &self,
        id: &str,
    ) -> Result<Option<VersionedAgentConfig>, ConfigStoreError> {
        self.inner
            .get_config_versioned_scoped(&self.scope, id)
            .await
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        self.inner.list_configs_scoped(&self.scope).await
    }

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        self.inner
            .put_publication_scoped(&self.scope, publication)
            .await
    }

    async fn put_publication_if_config_generation(
        &self,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.inner
            .put_publication_if_config_generation_scoped(
                &self.scope,
                publication,
                expected_generation,
            )
            .await
    }

    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        self.inner
            .get_publication_scoped(&self.scope, fingerprint)
            .await
    }
}
