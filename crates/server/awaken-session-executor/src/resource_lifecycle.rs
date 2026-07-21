//! Ephemeral resource lifecycle adapter for an unconfigured embedded host.
//!
//! Durable deployments replace this through `SharedHost::with_resource_lifecycle`.
//! Keeping the default behind the same port avoids teaching runtime code about a
//! database while preserving zero-configuration tests and in-process use.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_protocol_managed::resource_plane::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourceReclamationFence, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};

#[derive(Default)]
struct ResourceConsistencyState {
    references: BTreeSet<ResourceReferenceRecord>,
    reclamation_fences: BTreeMap<(ResourceKind, String), String>,
}

#[derive(Default)]
pub(crate) struct EphemeralResourceLifecycle {
    intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
    consistency: Mutex<ResourceConsistencyState>,
}

fn storage(error: impl ToString) -> ResourcePurgeError {
    ResourcePurgeError::Storage(error.to_string())
}

fn valid_reference(record: &ResourceReferenceRecord) -> bool {
    !record.target.workspace_id.trim().is_empty()
        && !record.target.resource_id.trim().is_empty()
        && !record.reference.reference_id.trim().is_empty()
}

fn physical_key(target: &ResourceTarget) -> (ResourceKind, String) {
    (target.kind, target.resource_id.clone())
}

fn validate_fence_request(intent_id: &str, target: &ResourceTarget) -> bool {
    !intent_id.trim().is_empty()
        && !target.workspace_id.trim().is_empty()
        && !target.resource_id.trim().is_empty()
}

#[async_trait]
impl ResourceReclamationFence for EphemeralResourceLifecycle {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        if !validate_fence_request(intent_id, target) {
            return Err(ResourcePurgeError::Invalid(
                "reclamation fence fields must not be empty".into(),
            ));
        }
        let mut state = self.consistency.lock().map_err(storage)?;
        let key = physical_key(target);
        if let Some(owner) = state.reclamation_fences.get(&key) {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        let blockers: Vec<_> = state
            .references
            .iter()
            .filter(|record| {
                record.target.kind == target.kind && record.target.resource_id == target.resource_id
            })
            .cloned()
            .collect();
        if !blockers.is_empty() {
            return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
        }
        state.reclamation_fences.insert(key, intent_id.into());
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        if !validate_fence_request(intent_id, target) {
            return Err(ResourcePurgeError::Invalid(
                "reclamation fence fields must not be empty".into(),
            ));
        }
        let mut state = self.consistency.lock().map_err(storage)?;
        let key = physical_key(target);
        match state.reclamation_fences.get(&key) {
            Some(owner) if owner == intent_id => {
                state.reclamation_fences.remove(&key);
                Ok(true)
            }
            Some(_) => Err(ResourcePurgeError::StaleReclamationFence),
            None => Ok(false),
        }
    }
}

#[async_trait]
impl ResourcePurgeRepository for EphemeralResourceLifecycle {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        intent.validate()?;
        let mut rows = self.intents.lock().map_err(storage)?;
        if let Some(existing) = rows.get(&intent.intent_id).or_else(|| {
            rows.values()
                .find(|row| row.idempotency_key == intent.idempotency_key)
        }) {
            return if existing.same_request(&intent) {
                Ok(PutResourcePurgeOutcome::Existing)
            } else {
                Err(ResourcePurgeError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        rows.insert(intent.intent_id.clone(), intent);
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(self
            .intents
            .lock()
            .map_err(storage)?
            .get(intent_id)
            .cloned())
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        let mut intents: Vec<_> = self
            .intents
            .lock()
            .map_err(storage)?
            .values()
            .filter(|intent| {
                !intent.status.is_terminal()
                    && intent.not_before_unix_ms <= now_unix_ms
                    && intent
                        .lease_expires_at_unix_ms
                        .is_none_or(|expires| expires <= now_unix_ms)
            })
            .cloned()
            .collect();
        intents.sort_by(|a, b| {
            a.requested_at_unix_ms
                .cmp(&b.requested_at_unix_ms)
                .then_with(|| a.intent_id.cmp(&b.intent_id))
        });
        intents.truncate(limit);
        Ok(intents)
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        let mut rows = self.intents.lock().map_err(storage)?;
        let current = rows
            .get(&intent.intent_id)
            .ok_or_else(|| ResourcePurgeError::NotFound(intent.intent_id.clone()))?;
        if current.revision != expected_revision {
            return Err(ResourcePurgeError::RevisionConflict(intent.intent_id));
        }
        rows.insert(intent.intent_id.clone(), intent);
        Ok(())
    }
}

#[async_trait]
impl ResourceReferenceIndex for EphemeralResourceLifecycle {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        if !valid_reference(&record) {
            return Err(ResourcePurgeError::Invalid(
                "resource reference fields must not be empty".into(),
            ));
        }
        let mut state = self.consistency.lock().map_err(storage)?;
        if state
            .reclamation_fences
            .contains_key(&physical_key(&record.target))
        {
            return Err(ResourcePurgeError::ReclamationFenced {
                kind: record.target.kind,
                resource_id: record.target.resource_id,
            });
        }
        Ok(state.references.insert(record))
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self
            .consistency
            .lock()
            .map_err(storage)?
            .references
            .remove(record))
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        if reference_id.trim().is_empty()
            || records.iter().any(|record| {
                !valid_reference(record)
                    || record.reference.kind != kind
                    || record.reference.reference_id != reference_id
            })
        {
            return Err(ResourcePurgeError::Invalid(
                "invalid resource reference replacement".into(),
            ));
        }
        let mut state = self.consistency.lock().map_err(storage)?;
        if let Some(record) = records.iter().find(|record| {
            state
                .reclamation_fences
                .contains_key(&physical_key(&record.target))
        }) {
            return Err(ResourcePurgeError::ReclamationFenced {
                kind: record.target.kind,
                resource_id: record.target.resource_id.clone(),
            });
        }
        state.references.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        state.references.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(self
            .consistency
            .lock()
            .map_err(storage)?
            .references
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
            .consistency
            .lock()
            .map_err(storage)?
            .references
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}
