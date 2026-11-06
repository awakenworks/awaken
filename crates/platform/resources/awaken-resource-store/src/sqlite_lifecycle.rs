// @formal entrypoint resource_reclamation.physical_purge_requires_zero_references_leases_and_fence
// @formal entrypoint resource_reclamation.reference_and_lease_races_fail_closed
//! SQLite Resource lifecycle and reference-index adapter implementations.

use async_trait::async_trait;
use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourceReclamationFence, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use sqlx::SqliteConnection;

use crate::{
    ReclamationAcquisitionPlan, SqliteResourceStore, decode_intent, encode_intent, kind_name,
    parse_reference_kind, plan_reclamation_acquisition, prepare_replacement, reference_kind_name,
    reference_params, replacement_coordinates, status_name, storage, to_i64,
    validate_fence_request, validate_reference,
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
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| storage(error.to_string()))?;
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT intent_id FROM resource_lifecycle_reclamation_fences
             WHERE resource_kind = ?1 AND resource_id = ?2",
        )
        .bind(kind_name(target.kind))
        .bind(&target.resource_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        let blockers = if existing.is_none() {
            sqlite_references_for_identity(&mut transaction, target.kind, &target.resource_id)
                .await?
        } else {
            Vec::new()
        };
        match plan_reclamation_acquisition(existing.as_deref(), intent_id, blockers) {
            ReclamationAcquisitionPlan::Return(outcome) => return Ok(outcome),
            ReclamationAcquisitionPlan::InstallFence => {}
        }
        sqlx::query(
            "INSERT INTO resource_lifecycle_reclamation_fences
             (resource_kind, resource_id, intent_id) VALUES (?1, ?2, ?3)",
        )
        .bind(kind_name(target.kind))
        .bind(&target.resource_id)
        .bind(intent_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        // A trigger, migration hook, or corrupted same-transaction writer can
        // add a reference after the pre-insert scan. Ordinary concurrent writers
        // are already serialized by the transaction/fence, but this second read
        // closes the storage-local interval without recreating an application
        // guard. Commit the late reference while removing only our fence.
        let blockers =
            sqlite_references_for_identity(&mut transaction, target.kind, &target.resource_id)
                .await?;
        if !blockers.is_empty() {
            sqlx::query(
                "DELETE FROM resource_lifecycle_reclamation_fences
                 WHERE resource_kind = ?1 AND resource_id = ?2 AND intent_id = ?3",
            )
            .bind(kind_name(target.kind))
            .bind(&target.resource_id)
            .bind(intent_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage(error.to_string()))?;
            transaction
                .commit()
                .await
                .map_err(|error| storage(error.to_string()))?;
            return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage(error.to_string()))?;
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| storage(error.to_string()))?;
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT intent_id FROM resource_lifecycle_reclamation_fences
             WHERE resource_kind = ?1 AND resource_id = ?2",
        )
        .bind(kind_name(target.kind))
        .bind(&target.resource_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        let changed = match existing {
            Some(owner) if owner == intent_id => {
                sqlx::query(
                    "DELETE FROM resource_lifecycle_reclamation_fences
                         WHERE resource_kind = ?1 AND resource_id = ?2 AND intent_id = ?3",
                )
                .bind(kind_name(target.kind))
                .bind(&target.resource_id)
                .bind(intent_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| storage(error.to_string()))?
                .rows_affected()
                    == 1
            }
            Some(_) => return Err(ResourcePurgeError::StaleReclamationFence),
            None => false,
        };
        transaction
            .commit()
            .await
            .map_err(|error| storage(error.to_string()))?;
        Ok(changed)
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
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| storage(error.to_string()))?;
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT data FROM resource_lifecycle_purge_intents
             WHERE intent_id = ?1 OR idempotency_key = ?2 LIMIT 1",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .fetch_optional(&mut *transaction)
        .await
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
        sqlx::query(
            "INSERT INTO resource_lifecycle_purge_intents
             (intent_id, idempotency_key, revision, status, requested_at_unix_ms,
              not_before_unix_ms, lease_expires_at_unix_ms, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(to_i64(intent.revision)?)
        .bind(status_name(intent.status))
        .bind(to_i64(intent.requested_at_unix_ms)?)
        .bind(to_i64(intent.not_before_unix_ms)?)
        .bind(intent.lease_expires_at_unix_ms.map(to_i64).transpose()?)
        .bind(data)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        transaction
            .commit()
            .await
            .map_err(|error| storage(error.to_string()))?;
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        sqlx::query_scalar::<_, String>(
            "SELECT data FROM resource_lifecycle_purge_intents WHERE intent_id = ?1",
        )
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .map(|data| decode_intent(&data))
        .transpose()
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT data FROM resource_lifecycle_purge_intents
             WHERE status NOT IN ('completed', 'terminal_failed')
               AND not_before_unix_ms <= ?1
               AND (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms <= ?1)
             ORDER BY requested_at_unix_ms, intent_id LIMIT ?2",
        )
        .bind(to_i64(now_unix_ms)?)
        .bind(to_i64(limit as u64)?)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?;
        rows.iter().map(|data| decode_intent(data)).collect()
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        let data = encode_intent(&intent)?;
        let changed = sqlx::query(
            "UPDATE resource_lifecycle_purge_intents
             SET revision = ?3, status = ?4, lease_expires_at_unix_ms = ?5, data = ?6
             WHERE intent_id = ?1 AND revision = ?2",
        )
        .bind(&intent.intent_id)
        .bind(to_i64(expected_revision)?)
        .bind(to_i64(intent.revision)?)
        .bind(status_name(intent.status))
        .bind(intent.lease_expires_at_unix_ms.map(to_i64).transpose()?)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .rows_affected();
        if changed == 1 {
            return Ok(());
        }
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM resource_lifecycle_purge_intents WHERE intent_id = ?1",
        )
        .bind(&intent.intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .is_some();
        if exists {
            Err(ResourcePurgeError::RevisionConflict(intent.intent_id))
        } else {
            Err(ResourcePurgeError::NotFound(intent.intent_id))
        }
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
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| storage(error.to_string()))?;
        sqlite_ensure_unfenced(&mut transaction, &record.target).await?;
        let [
            workspace_id,
            resource_kind,
            resource_id,
            reference_kind,
            reference_id,
        ] = reference_params(&record);
        let changed = sqlx::query(
            "INSERT OR IGNORE INTO resource_lifecycle_references
             (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(workspace_id)
        .bind(resource_kind)
        .bind(resource_id)
        .bind(reference_kind)
        .bind(reference_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?
        .rows_affected()
            == 1;
        transaction
            .commit()
            .await
            .map_err(|error| storage(error.to_string()))?;
        Ok(changed)
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(record)?;
        let [
            workspace_id,
            resource_kind,
            resource_id,
            reference_kind,
            reference_id,
        ] = reference_params(record);
        Ok(sqlx::query(
            "DELETE FROM resource_lifecycle_references
             WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
               AND reference_kind = ?4 AND reference_id = ?5",
        )
        .bind(workspace_id)
        .bind(resource_kind)
        .bind(resource_id)
        .bind(reference_kind)
        .bind(reference_id)
        .execute(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .rows_affected()
            == 1)
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        let records = prepare_replacement(kind, reference_id, records)?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| storage(error.to_string()))?;
        for record in &records {
            sqlite_ensure_unfenced(&mut transaction, &record.target).await?;
        }
        let existing = sqlx::query_as::<_, (String, String, String)>(
            "SELECT workspace_id, resource_kind, resource_id
             FROM resource_lifecycle_references
             WHERE reference_kind = ?1 AND reference_id = ?2",
        )
        .bind(reference_kind_name(kind))
        .bind(reference_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        if existing == replacement_coordinates(&records) {
            transaction
                .commit()
                .await
                .map_err(|error| storage(error.to_string()))?;
            return Ok(());
        }
        sqlx::query(
            "DELETE FROM resource_lifecycle_references WHERE reference_kind = ?1 AND reference_id = ?2",
        )
        .bind(reference_kind_name(kind))
        .bind(reference_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        for record in records {
            let [
                workspace_id,
                resource_kind,
                resource_id,
                reference_kind,
                reference_id,
            ] = reference_params(&record);
            sqlx::query(
                "INSERT INTO resource_lifecycle_references
                 (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(workspace_id)
            .bind(resource_kind)
            .bind(resource_id)
            .bind(reference_kind)
            .bind(reference_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage(error.to_string()))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| storage(error.to_string()))
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT reference_kind, reference_id FROM resource_lifecycle_references
             WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
             ORDER BY reference_kind, reference_id",
        )
        .bind(&target.workspace_id)
        .bind(kind_name(target.kind))
        .bind(&target.resource_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?;
        rows.into_iter()
            .map(|(kind, reference_id)| {
                Ok(ResourceReference {
                    kind: parse_reference_kind(&kind)?,
                    reference_id,
                })
            })
            .collect()
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| storage(error.to_string()))?;
        sqlite_references_for_identity(&mut connection, kind, resource_id).await
    }
}

#[cfg(feature = "sqlite")]
async fn sqlite_ensure_unfenced(
    connection: &mut SqliteConnection,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM resource_lifecycle_reclamation_fences
             WHERE resource_kind = ?1 AND resource_id = ?2",
    )
    .bind(kind_name(target.kind))
    .bind(&target.resource_id)
    .fetch_optional(&mut *connection)
    .await
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
async fn sqlite_references_for_identity(
    connection: &mut SqliteConnection,
    kind: ResourceKind,
    resource_id: &str,
) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT workspace_id, reference_kind, reference_id FROM resource_lifecycle_references
             WHERE resource_kind = ?1 AND resource_id = ?2
             ORDER BY workspace_id, reference_kind, reference_id",
    )
    .bind(kind_name(kind))
    .bind(resource_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| storage(error.to_string()))?;
    rows.into_iter()
        .map(|(workspace_id, reference_kind, reference_id)| {
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
