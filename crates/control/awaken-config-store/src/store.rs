//! The `ConfigStore` port and its persisted aggregates.

use awaken_runtime_contract::catalog::RuntimeCatalogInstall;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use serde::{Deserialize, Serialize};

use crate::compile::Publication;
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
    /// Wrap a freshly compiled publication as `published`.
    pub fn published(publication: Publication, agent_id: impl Into<String>) -> Self {
        Self {
            publication_id: publication.publication_id,
            fingerprint: publication.fingerprint,
            agent_id: agent_id.into(),
            state: PublicationState::Published,
            snapshot: publication.snapshot,
            install: publication.install,
        }
    }
}

/// Durable storage for the config domain: agent configs (the authoring
/// aggregate) and publications (the compiled artifact). Adapters live under the
/// `config` table namespace, alongside the runtime's tables (ADR-0029).
#[async_trait::async_trait]
pub trait ConfigStore: Send + Sync {
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
