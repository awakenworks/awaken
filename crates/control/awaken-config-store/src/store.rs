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
    pub state: PublicationState,
    pub snapshot: ExecutableAgentSnapshot,
    pub install: RuntimeCatalogInstall,
}

impl StoredPublication {
    /// Wrap a freshly compiled config as `published`. The fingerprint and
    /// publication id come from the runnable config itself (the producer stamped
    /// them), so the store never re-derives them.
    pub fn published(config: RunnableConfig, agent_id: impl Into<String>) -> Self {
        let (snapshot, install) = config.into_parts();
        Self {
            publication_id: install.publication_id.clone(),
            fingerprint: snapshot.fingerprint.0.clone(),
            agent_id: agent_id.into(),
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

    /// Load an agent config by id.
    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError>;

    /// Store a publication, idempotent by fingerprint.
    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

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

    /// Load an agent config by id **within `scope`** — a row owned by another
    /// scope is invisible.
    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError>;

    /// Store a publication owned by `scope`, idempotent by fingerprint.
    async fn put_publication_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

    /// Load a publication by fingerprint **within `scope`**.
    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError>;
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

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.inner.get_config_scoped(&self.scope, id).await
    }

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        self.inner
            .put_publication_scoped(&self.scope, publication)
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
