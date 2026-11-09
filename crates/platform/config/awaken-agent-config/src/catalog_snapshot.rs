// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This read port observes the existing Config owner and its monotonic
// @formal reason: scope epoch. It grants no identity, Agent use or definition authority.
//! A consistent bounded read of current and executable Config revisions.
use crate::personal_selection::{AgentSelectionOwner, AgentSelectionState};
use crate::{AgentConfigRevision, ConfigStoreError, ScopeId, StoredPublication};

#[derive(Debug, Clone)]
pub struct AgentCatalogEntry {
    pub head: AgentConfigRevision,
    pub published: Option<AgentConfigRevision>,
    pub publication: Option<StoredPublication>,
}

#[derive(Debug, Clone)]
pub struct AgentCatalogPreference {
    pub agent_id: String,
    pub state: AgentSelectionState,
}

/// This observation is not an authorization receipt or a cursor. An absent
/// epoch means the scope has never held catalog state, not numeric zero.
#[derive(Debug, Clone)]
pub struct AgentCatalogSnapshot {
    pub epoch: Option<u64>,
    pub entries: Vec<AgentCatalogEntry>,
    pub preferences: Vec<AgentCatalogPreference>,
}

/// Mandatory views of the same physical Config adapter. A caller still performs
/// current native authorization for every object before projecting a response.
#[async_trait::async_trait]
pub trait AgentCatalogSnapshotRepository: Send + Sync {
    async fn agent_catalog_snapshot(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        maximum_agents: usize,
    ) -> Result<AgentCatalogSnapshot, ConfigStoreError>;

    async fn agent_catalog_epoch(&self, scope: &ScopeId) -> Result<Option<u64>, ConfigStoreError>;
}
