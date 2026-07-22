use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use awaken_protocol_managed::resource_plane::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind,
    ResourceLifecycleRepository, ResourcePurgeError, ResourcePurgeIntent, ResourcePurgeRepository,
    ResourceReclamationFence, ResourceReference, ResourceReferenceIndex, ResourceReferenceKind,
    ResourceReferenceRecord, ResourceTarget,
};

#[derive(Default)]
struct TestResourceLifecycle {
    intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
    references: Mutex<BTreeSet<ResourceReferenceRecord>>,
    fences: Mutex<BTreeMap<(ResourceKind, String), String>>,
}

#[async_trait::async_trait]
impl ResourcePurgeRepository for TestResourceLifecycle {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        let mut intents = self.intents.lock().unwrap();
        if let Some(existing) = intents.get(&intent.intent_id) {
            return if existing.same_request(&intent) {
                Ok(PutResourcePurgeOutcome::Existing)
            } else {
                Err(ResourcePurgeError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        intents.insert(intent.intent_id.clone(), intent);
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(self.intents.lock().unwrap().get(intent_id).cloned())
    }

    async fn recoverable(
        &self,
        _now_unix_ms: u64,
        _limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(Vec::new())
    }

    async fn save(
        &self,
        _expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        self.intents
            .lock()
            .unwrap()
            .insert(intent.intent_id.clone(), intent);
        Ok(())
    }
}

#[async_trait::async_trait]
impl ResourceReferenceIndex for TestResourceLifecycle {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self.references.lock().unwrap().insert(record))
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self.references.lock().unwrap().remove(record))
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        let mut references = self.references.lock().unwrap();
        references.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        references.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(self
            .references
            .lock()
            .unwrap()
            .iter()
            .filter(|record| &record.target == target)
            .map(|record| record.reference.clone())
            .collect())
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        Ok(self
            .references
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl ResourceReclamationFence for TestResourceLifecycle {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if let Some(owner) = fences.get(&key) {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        fences.insert(key, intent_id.into());
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if fences.get(&key).is_some_and(|owner| owner == intent_id) {
            fences.remove(&key);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn resource_lifecycle() -> Arc<dyn ResourceLifecycleRepository> {
    Arc::new(TestResourceLifecycle::default())
}
