//! In-memory durable-delegation repository used by embedded ingress and tests.
//! Database implementations use the same compare-and-set contract.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::agent::delegation::DelegationGroup;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_run_ingress_contract::{
    DelegationCas, DelegationStore, DelegationStoreError, StoredDelegationGroup,
};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct MemoryDelegationStore {
    groups: Mutex<BTreeMap<String, StoredDelegationGroup>>,
}

impl MemoryDelegationStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DelegationStore for MemoryDelegationStore {
    async fn create(&self, group: DelegationGroup) -> Result<(), DelegationStoreError> {
        group
            .validate()
            .map_err(|error| DelegationStoreError::Rejected(error.to_string()))?;
        let key = group.parent_run_id().0.clone();
        let mut groups = self.groups.lock().await;
        match groups.get(&key) {
            Some(existing) if existing.group == group => Ok(()),
            Some(_) => Err(DelegationStoreError::Conflict),
            None => {
                groups.insert(key, StoredDelegationGroup { revision: 0, group });
                Ok(())
            }
        }
    }

    async fn load(
        &self,
        parent_run_id: &RunId,
    ) -> Result<Option<StoredDelegationGroup>, DelegationStoreError> {
        Ok(self.groups.lock().await.get(&parent_run_id.0).cloned())
    }

    async fn compare_and_set(
        &self,
        expected_revision: u64,
        group: DelegationGroup,
    ) -> Result<DelegationCas, DelegationStoreError> {
        group
            .validate()
            .map_err(|error| DelegationStoreError::Rejected(error.to_string()))?;
        let key = group.parent_run_id().0.clone();
        let mut groups = self.groups.lock().await;
        let stored = groups.get_mut(&key).ok_or(DelegationStoreError::NotFound)?;
        if stored.revision != expected_revision {
            return Ok(DelegationCas::Fenced {
                current_revision: stored.revision,
            });
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or(DelegationStoreError::RevisionOverflow)?;
        *stored = StoredDelegationGroup { revision, group };
        Ok(DelegationCas::Applied { revision })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::delegation::{
        DelegationId, DelegationKind, DelegationLimits, RequestDelegation,
    };

    fn group() -> DelegationGroup {
        DelegationGroup::new(
            RunId("parent".into()),
            "coordinator",
            Vec::new(),
            0,
            DelegationLimits::new(3, 4, 8),
        )
    }

    fn request(id: &str) -> RequestDelegation {
        RequestDelegation {
            id: DelegationId(id.into()),
            parent_call_id: format!("call-{id}"),
            target_agent_id: format!("agent-{id}"),
            child_run_id: RunId(format!("run-{id}")),
            kind: DelegationKind::Local,
        }
    }

    #[tokio::test]
    async fn stale_parent_or_child_process_is_fenced_after_independent_recovery() {
        let store = MemoryDelegationStore::new();
        store.create(group()).await.unwrap();
        let parent_process = store.load(&RunId("parent".into())).await.unwrap().unwrap();
        let child_process = parent_process.clone();

        let mut parent_update = parent_process.group;
        parent_update.request(request("a")).unwrap();
        assert_eq!(
            store
                .compare_and_set(parent_process.revision, parent_update)
                .await
                .unwrap(),
            DelegationCas::Applied { revision: 1 }
        );

        let mut stale_child_update = child_process.group;
        stale_child_update.request(request("b")).unwrap();
        assert_eq!(
            store
                .compare_and_set(child_process.revision, stale_child_update)
                .await
                .unwrap(),
            DelegationCas::Fenced {
                current_revision: 1
            }
        );
    }

    #[tokio::test]
    async fn persisted_group_survives_repository_reconstruction() {
        let store = MemoryDelegationStore::new();
        let mut original = group();
        original.request(request("a")).unwrap();
        store.create(original.clone()).await.unwrap();

        let bytes =
            serde_json::to_vec(&store.load(&RunId("parent".into())).await.unwrap().unwrap())
                .unwrap();
        let recovered: StoredDelegationGroup = serde_json::from_slice(&bytes).unwrap();
        recovered.group.validate().unwrap();
        assert_eq!(recovered.group, original);
    }
}
