//! Ephemeral resource lifecycle adapter for an unconfigured embedded host.
//!
//! Durable deployments replace this through `SharedHost::with_resource_lifecycle`.
//! Keeping the default behind the same port avoids teaching runtime code about a
//! database while preserving zero-configuration tests and in-process use.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_protocol_managed::resource_plane::{
    PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError, ResourcePurgeIntent,
    ResourcePurgeRepository, ResourceReference, ResourceReferenceIndex, ResourceReferenceKind,
    ResourceReferenceRecord, ResourceTarget,
};

#[derive(Default)]
pub(crate) struct EphemeralResourceLifecycle {
    intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
    references: Mutex<BTreeSet<ResourceReferenceRecord>>,
}

fn storage(error: impl ToString) -> ResourcePurgeError {
    ResourcePurgeError::Storage(error.to_string())
}

fn valid_reference(record: &ResourceReferenceRecord) -> bool {
    !record.target.workspace_id.trim().is_empty()
        && !record.target.resource_id.trim().is_empty()
        && !record.reference.reference_id.trim().is_empty()
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
        Ok(self.references.lock().map_err(storage)?.insert(record))
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self.references.lock().map_err(storage)?.remove(record))
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
        let mut current = self.references.lock().map_err(storage)?;
        current.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        current.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(self
            .references
            .lock()
            .map_err(storage)?
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
            .map_err(storage)?
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}
