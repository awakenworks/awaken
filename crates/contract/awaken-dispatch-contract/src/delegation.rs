//! Durable storage contract for parent/child Run coordination.
//!
//! The domain transitions live in `DelegationGroup`; this repository adds only
//! optimistic concurrency. A process must load, apply one domain transition,
//! then compare-and-set. A stale process is fenced and must reload.

use async_trait::async_trait;
use awaken_agent_contract::agent::delegation::DelegationGroup;
use awaken_agent_contract::agent::run::Id as RunId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDelegationGroup {
    pub revision: u64,
    pub group: DelegationGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationCas {
    Applied { revision: u64 },
    Fenced { current_revision: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum DelegationStoreError {
    #[error("delegation store rejected: {0}")]
    Rejected(String),
    #[error("delegation group already exists with different content")]
    Conflict,
    #[error("delegation group does not exist")]
    NotFound,
    #[error("delegation revision overflowed")]
    RevisionOverflow,
}

#[async_trait]
pub trait DelegationStore: Send + Sync {
    /// Idempotently create revision zero for a parent Run.
    async fn create(&self, group: DelegationGroup) -> Result<(), DelegationStoreError>;

    async fn load(
        &self,
        parent_run_id: &RunId,
    ) -> Result<Option<StoredDelegationGroup>, DelegationStoreError>;

    /// Replace a group only when `expected_revision` is still current.
    async fn compare_and_set(
        &self,
        expected_revision: u64,
        group: DelegationGroup,
    ) -> Result<DelegationCas, DelegationStoreError>;
}
