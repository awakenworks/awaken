//! SQLite Resource lifecycle and reference-index adapter implementations.

use async_trait::async_trait;
use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourceReclamationFence, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    SqliteResourceStore, decode_intent, encode_intent, kind_name, parse_reference_kind,
    prepare_replacement, reference_kind_name, reference_params, replacement_coordinates,
    status_name, storage, to_i64, validate_fence_request, validate_reference,
};

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourceReclamationFence for SqliteResourceStore {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let intent_id = intent_id.to_owned();
        let target = target.clone();
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage(error.to_string()))?;
            let existing = transaction
                .query_row(
                    "SELECT intent_id FROM resource_lifecycle_reclamation_fences
                     WHERE resource_kind = ?1 AND resource_id = ?2",
                    params![kind_name(target.kind), target.resource_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| storage(error.to_string()))?;
            if let Some(owner) = existing {
                return Ok(if owner == intent_id {
                    AcquireResourceReclamationOutcome::AlreadyOwned
                } else {
                    AcquireResourceReclamationOutcome::Contended
                });
            }
            let blockers =
                sqlite_references_for_identity(&transaction, target.kind, &target.resource_id)?;
            if !blockers.is_empty() {
                return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
            }
            transaction
                .execute(
                    "INSERT INTO resource_lifecycle_reclamation_fences
                     (resource_kind, resource_id, intent_id) VALUES (?1, ?2, ?3)",
                    params![kind_name(target.kind), target.resource_id, intent_id],
                )
                .map_err(|error| storage(error.to_string()))?;
            // A trigger, migration hook, or corrupted same-transaction writer can
            // add a reference after the pre-insert scan. Ordinary concurrent writers
            // are already serialized by the transaction/fence, but this second read
            // closes the storage-local interval without recreating an application
            // guard. Commit the late reference while removing only our fence.
            let blockers =
                sqlite_references_for_identity(&transaction, target.kind, &target.resource_id)?;
            if !blockers.is_empty() {
                transaction
                    .execute(
                        "DELETE FROM resource_lifecycle_reclamation_fences
                         WHERE resource_kind = ?1 AND resource_id = ?2 AND intent_id = ?3",
                        params![kind_name(target.kind), target.resource_id, intent_id],
                    )
                    .map_err(|error| storage(error.to_string()))?;
                transaction
                    .commit()
                    .map_err(|error| storage(error.to_string()))?;
                return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
            }
            transaction
                .commit()
                .map_err(|error| storage(error.to_string()))?;
            Ok(AcquireResourceReclamationOutcome::Acquired)
        })
        .await
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let intent_id = intent_id.to_owned();
        let target = target.clone();
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage(error.to_string()))?;
            let existing = transaction
                .query_row(
                    "SELECT intent_id FROM resource_lifecycle_reclamation_fences
                     WHERE resource_kind = ?1 AND resource_id = ?2",
                    params![kind_name(target.kind), target.resource_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| storage(error.to_string()))?;
            let changed = match existing {
                Some(owner) if owner == intent_id => {
                    transaction
                        .execute(
                            "DELETE FROM resource_lifecycle_reclamation_fences
                         WHERE resource_kind = ?1 AND resource_id = ?2 AND intent_id = ?3",
                            params![kind_name(target.kind), target.resource_id, intent_id],
                        )
                        .map_err(|error| storage(error.to_string()))?
                        == 1
                }
                Some(_) => return Err(ResourcePurgeError::StaleReclamationFence),
                None => false,
            };
            transaction
                .commit()
                .map_err(|error| storage(error.to_string()))?;
            Ok(changed)
        })
        .await
    }
}

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourcePurgeRepository for SqliteResourceStore {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        intent.validate()?;
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage(error.to_string()))?;
            let existing = transaction
                .query_row(
                    "SELECT data FROM resource_lifecycle_purge_intents
                     WHERE intent_id = ?1 OR idempotency_key = ?2 LIMIT 1",
                    params![intent.intent_id, intent.idempotency_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| storage(error.to_string()))?;
            if let Some(data) = existing {
                let existing = decode_intent(&data)?;
                return if existing.same_request(&intent) {
                    Ok(PutResourcePurgeOutcome::Existing)
                } else {
                    Err(ResourcePurgeError::IdempotencyConflict(
                        intent.idempotency_key,
                    ))
                };
            }
            let data = encode_intent(&intent)?;
            transaction
                .execute(
                    "INSERT INTO resource_lifecycle_purge_intents
                     (intent_id, idempotency_key, revision, status, requested_at_unix_ms,
                      not_before_unix_ms, lease_expires_at_unix_ms, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        intent.intent_id,
                        intent.idempotency_key,
                        to_i64(intent.revision)?,
                        status_name(intent.status),
                        to_i64(intent.requested_at_unix_ms)?,
                        to_i64(intent.not_before_unix_ms)?,
                        intent.lease_expires_at_unix_ms.map(to_i64).transpose()?,
                        data,
                    ],
                )
                .map_err(|error| storage(error.to_string()))?;
            transaction
                .commit()
                .map_err(|error| storage(error.to_string()))?;
            Ok(PutResourcePurgeOutcome::Inserted)
        })
        .await
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        let intent_id = intent_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT data FROM resource_lifecycle_purge_intents WHERE intent_id = ?1",
                    params![intent_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| storage(error.to_string()))?
                .map(|data| decode_intent(&data))
                .transpose()
        })
        .await
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT data FROM resource_lifecycle_purge_intents
                     WHERE status NOT IN ('completed', 'terminal_failed')
                       AND not_before_unix_ms <= ?1
                       AND (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms <= ?1)
                     ORDER BY requested_at_unix_ms, intent_id LIMIT ?2",
                )
                .map_err(|error| storage(error.to_string()))?;
            let rows = statement
                .query_map(
                    params![to_i64(now_unix_ms)?, to_i64(limit as u64)?],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| storage(error.to_string()))?;
            rows.map(|row| {
                let data = row.map_err(|error| storage(error.to_string()))?;
                decode_intent(&data)
            })
            .collect()
        })
        .await
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        self.with_connection(move |connection| {
            let data = encode_intent(&intent)?;
            let changed = connection
                .execute(
                    "UPDATE resource_lifecycle_purge_intents
                     SET revision = ?3, status = ?4, lease_expires_at_unix_ms = ?5, data = ?6
                     WHERE intent_id = ?1 AND revision = ?2",
                    params![
                        intent.intent_id,
                        to_i64(expected_revision)?,
                        to_i64(intent.revision)?,
                        status_name(intent.status),
                        intent.lease_expires_at_unix_ms.map(to_i64).transpose()?,
                        data,
                    ],
                )
                .map_err(|error| storage(error.to_string()))?;
            if changed == 1 {
                return Ok(());
            }
            let exists = connection
                .query_row(
                    "SELECT 1 FROM resource_lifecycle_purge_intents WHERE intent_id = ?1",
                    params![intent.intent_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| storage(error.to_string()))?
                .is_some();
            if exists {
                Err(ResourcePurgeError::RevisionConflict(intent.intent_id))
            } else {
                Err(ResourcePurgeError::NotFound(intent.intent_id))
            }
        })
        .await
    }
}

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourceReferenceIndex for SqliteResourceStore {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(&record)?;
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage(error.to_string()))?;
            sqlite_ensure_unfenced(&transaction, &record.target)?;
            let changed = transaction
                .execute(
                    "INSERT OR IGNORE INTO resource_lifecycle_references
                     (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    reference_params(&record),
                )
                .map_err(|error| storage(error.to_string()))?
                == 1;
            transaction
                .commit()
                .map_err(|error| storage(error.to_string()))?;
            Ok(changed)
        })
        .await
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(record)?;
        let record = record.clone();
        self.with_connection(move |connection| {
            Ok(connection
                .execute(
                    "DELETE FROM resource_lifecycle_references
                     WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
                       AND reference_kind = ?4 AND reference_id = ?5",
                    reference_params(&record),
                )
                .map_err(|error| storage(error.to_string()))?
                == 1)
        })
        .await
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        let records = prepare_replacement(kind, reference_id, records)?;
        let reference_id = reference_id.to_owned();
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| storage(error.to_string()))?;
            for record in &records {
                sqlite_ensure_unfenced(&transaction, &record.target)?;
            }
            let existing = {
                let mut statement = transaction
                    .prepare(
                        "SELECT workspace_id, resource_kind, resource_id
                         FROM resource_lifecycle_references
                         WHERE reference_kind = ?1 AND reference_id = ?2",
                    )
                    .map_err(|error| storage(error.to_string()))?;
                statement
                    .query_map(params![reference_kind_name(kind), reference_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .map_err(|error| storage(error.to_string()))?
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()
                    .map_err(|error| storage(error.to_string()))?
            };
            if existing == replacement_coordinates(&records) {
                transaction
                    .commit()
                    .map_err(|error| storage(error.to_string()))?;
                return Ok(());
            }
            transaction
                .execute(
                    "DELETE FROM resource_lifecycle_references WHERE reference_kind = ?1 AND reference_id = ?2",
                    params![reference_kind_name(kind), reference_id],
                )
                .map_err(|error| storage(error.to_string()))?;
            for record in records {
                transaction
                    .execute(
                        "INSERT INTO resource_lifecycle_references
                         (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        reference_params(&record),
                    )
                    .map_err(|error| storage(error.to_string()))?;
            }
            transaction
                .commit()
                .map_err(|error| storage(error.to_string()))
        })
        .await
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let target = target.clone();
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT reference_kind, reference_id FROM resource_lifecycle_references
                     WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
                     ORDER BY reference_kind, reference_id",
                )
                .map_err(|error| storage(error.to_string()))?;
            let rows = statement
                .query_map(
                    params![
                        target.workspace_id,
                        kind_name(target.kind),
                        target.resource_id
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(|error| storage(error.to_string()))?;
            rows.map(|row| {
                let (kind, reference_id) = row.map_err(|error| storage(error.to_string()))?;
                Ok(ResourceReference {
                    kind: parse_reference_kind(&kind)?,
                    reference_id,
                })
            })
            .collect()
        })
        .await
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        let resource_id = resource_id.to_owned();
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT workspace_id, reference_kind, reference_id FROM resource_lifecycle_references
                     WHERE resource_kind = ?1 AND resource_id = ?2
                     ORDER BY workspace_id, reference_kind, reference_id",
                )
                .map_err(|error| storage(error.to_string()))?;
            let rows = statement
                .query_map(params![kind_name(kind), resource_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| storage(error.to_string()))?;
            rows.map(|row| {
                let (workspace_id, reference_kind, reference_id) =
                    row.map_err(|error| storage(error.to_string()))?;
                Ok(ResourceReferenceRecord {
                    target: ResourceTarget::new(workspace_id, kind, resource_id.clone()),
                    reference: ResourceReference {
                        kind: parse_reference_kind(&reference_kind)?,
                        reference_id,
                    },
                })
            })
            .collect()
        })
        .await
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_ensure_unfenced(
    connection: &Connection,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM resource_lifecycle_reclamation_fences
             WHERE resource_kind = ?1 AND resource_id = ?2",
            params![kind_name(target.kind), target.resource_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| storage(error.to_string()))?
        .is_some();
    if exists {
        Err(ResourcePurgeError::ReclamationFenced {
            kind: target.kind,
            resource_id: target.resource_id.clone(),
        })
    } else {
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_references_for_identity(
    connection: &Connection,
    kind: ResourceKind,
    resource_id: &str,
) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
    let mut statement = connection
        .prepare(
            "SELECT workspace_id, reference_kind, reference_id FROM resource_lifecycle_references
             WHERE resource_kind = ?1 AND resource_id = ?2
             ORDER BY workspace_id, reference_kind, reference_id",
        )
        .map_err(|error| storage(error.to_string()))?;
    let rows = statement
        .query_map(params![kind_name(kind), resource_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| storage(error.to_string()))?;
    rows.map(|row| {
        let (workspace_id, reference_kind, reference_id) =
            row.map_err(|error| storage(error.to_string()))?;
        Ok(ResourceReferenceRecord {
            target: ResourceTarget::new(workspace_id, kind, resource_id),
            reference: ResourceReference {
                kind: parse_reference_kind(&reference_kind)?,
                reference_id,
            },
        })
    })
    .collect()
}
