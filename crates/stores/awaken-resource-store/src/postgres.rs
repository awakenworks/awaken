//! PostgreSQL resource-lifecycle adapter for multi-node compositions.
//!
//! Advisory transaction locks serialize reference creation with fence changes;
//! the durable fence row survives process loss while physical deletion is in
//! progress. Neither mechanism carries authentication or authorization data.

use std::collections::BTreeSet;

use async_trait::async_trait;
use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourceReclamationFence, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use sqlx::postgres::PgPool;
use sqlx::{Postgres, Row, Transaction};

use crate::schema::resource_lifecycle_bundle;
use crate::{
    decode_intent, encode_intent, kind_name, parse_reference_kind, reference_kind_name,
    status_name, storage, to_i64, validate_fence_request, validate_reference, validate_replacement,
};

const NS: &str = "resource_lifecycle";

/// Multi-node durable resource lifecycle state.
pub struct PostgresResourceStore {
    pool: PgPool,
}

impl PostgresResourceStore {
    /// Connect and apply the resource-lifecycle migration bundle.
    pub async fn connect(url: &str) -> Result<Self, ResourcePurgeError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| storage(format!("connect: {error}")))?;
        let store = Self::with_pool(pool);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap a shared pool without running migrations.
    #[must_use]
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the namespaced migration bundle idempotently.
    pub async fn ensure_schema(&self) -> Result<(), ResourcePurgeError> {
        let bundle = resource_lifecycle_bundle().map_err(|error| storage(error.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            self.pool.clone(),
            NS,
        )
        .map_err(|error| storage(error.to_string()))?
        .run_bundle(&bundle)
        .await
        .map(|_| ())
        .map_err(|error| storage(error.to_string()))
    }
}

async fn lock_identity(
    transaction: &mut Transaction<'_, Postgres>,
    kind: ResourceKind,
    resource_id: &str,
) -> Result<(), ResourcePurgeError> {
    let key = format!("{}\u{1f}{resource_id}", kind_name(kind));
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut **transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
    Ok(())
}

async fn fence_owner(
    transaction: &mut Transaction<'_, Postgres>,
    target: &ResourceTarget,
) -> Result<Option<String>, ResourcePurgeError> {
    sqlx::query(&format!(
        "SELECT intent_id FROM {NS}_reclamation_fences
         WHERE resource_kind = $1 AND resource_id = $2"
    ))
    .bind(kind_name(target.kind))
    .bind(&target.resource_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| storage(error.to_string()))?
    .map(|row| {
        row.try_get("intent_id")
            .map_err(|error| storage(error.to_string()))
    })
    .transpose()
}

async fn ensure_unfenced(
    transaction: &mut Transaction<'_, Postgres>,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    if fence_owner(transaction, target).await?.is_some() {
        Err(ResourcePurgeError::ReclamationFenced {
            kind: target.kind,
            resource_id: target.resource_id.clone(),
        })
    } else {
        Ok(())
    }
}

async fn references_for_identity(
    transaction: &mut Transaction<'_, Postgres>,
    kind: ResourceKind,
    resource_id: &str,
) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
    let rows = sqlx::query(&format!(
        "SELECT workspace_id, reference_kind, reference_id FROM {NS}_references
         WHERE resource_kind = $1 AND resource_id = $2
         ORDER BY workspace_id, reference_kind, reference_id"
    ))
    .bind(kind_name(kind))
    .bind(resource_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|error| storage(error.to_string()))?;
    rows.into_iter()
        .map(|row| {
            let workspace_id: String = row.try_get("workspace_id").map_err(storage)?;
            let reference_kind: String = row.try_get("reference_kind").map_err(storage)?;
            Ok(ResourceReferenceRecord {
                target: ResourceTarget::new(workspace_id, kind, resource_id),
                reference: ResourceReference {
                    kind: parse_reference_kind(&reference_kind)?,
                    reference_id: row.try_get("reference_id").map_err(storage)?,
                },
            })
        })
        .collect()
}

#[async_trait]
impl ResourceReclamationFence for PostgresResourceStore {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        lock_identity(&mut transaction, target.kind, &target.resource_id).await?;
        if let Some(owner) = fence_owner(&mut transaction, target).await? {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        let blockers =
            references_for_identity(&mut transaction, target.kind, &target.resource_id).await?;
        if !blockers.is_empty() {
            return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_reclamation_fences
             (resource_kind, resource_id, intent_id) VALUES ($1, $2, $3)"
        ))
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
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        lock_identity(&mut transaction, target.kind, &target.resource_id).await?;
        let changed = match fence_owner(&mut transaction, target).await? {
            Some(owner) if owner == intent_id => {
                sqlx::query(&format!(
                    "DELETE FROM {NS}_reclamation_fences
                 WHERE resource_kind = $1 AND resource_id = $2 AND intent_id = $3"
                ))
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
impl ResourcePurgeRepository for PostgresResourceStore {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        intent.validate()?;
        let data = encode_intent(&intent)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO {NS}_purge_intents
             (intent_id, idempotency_key, revision, status, requested_at_unix_ms,
              not_before_unix_ms, lease_expires_at_unix_ms, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING"
        ))
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
        .map_err(|error| storage(error.to_string()))?
        .rows_affected()
            == 1;
        if inserted {
            transaction
                .commit()
                .await
                .map_err(|error| storage(error.to_string()))?;
            return Ok(PutResourcePurgeOutcome::Inserted);
        }
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_purge_intents
             WHERE intent_id = $1 OR idempotency_key = $2 LIMIT 1"
        ))
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        let existing = decode_intent(row.try_get("data").map_err(storage)?)?;
        if existing.same_request(&intent) {
            Ok(PutResourcePurgeOutcome::Existing)
        } else {
            Err(ResourcePurgeError::IdempotencyConflict(
                intent.idempotency_key,
            ))
        }
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        sqlx::query(&format!(
            "SELECT data FROM {NS}_purge_intents WHERE intent_id = $1"
        ))
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .map(|row| {
            let data: String = row.try_get("data").map_err(storage)?;
            decode_intent(&data)
        })
        .transpose()
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_purge_intents
             WHERE status NOT IN ('completed', 'terminal_failed')
               AND not_before_unix_ms <= $1
               AND (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms <= $1)
             ORDER BY requested_at_unix_ms, intent_id LIMIT $2"
        ))
        .bind(to_i64(now_unix_ms)?)
        .bind(to_i64(limit as u64)?)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let data: String = row.try_get("data").map_err(storage)?;
                decode_intent(&data)
            })
            .collect()
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        let changed = sqlx::query(&format!(
            "UPDATE {NS}_purge_intents
             SET revision = $3, status = $4, lease_expires_at_unix_ms = $5, data = $6
             WHERE intent_id = $1 AND revision = $2"
        ))
        .bind(&intent.intent_id)
        .bind(to_i64(expected_revision)?)
        .bind(to_i64(intent.revision)?)
        .bind(status_name(intent.status))
        .bind(intent.lease_expires_at_unix_ms.map(to_i64).transpose()?)
        .bind(encode_intent(&intent)?)
        .execute(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else if self.get(&intent.intent_id).await?.is_some() {
            Err(ResourcePurgeError::RevisionConflict(intent.intent_id))
        } else {
            Err(ResourcePurgeError::NotFound(intent.intent_id))
        }
    }
}

#[async_trait]
impl ResourceReferenceIndex for PostgresResourceStore {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(&record)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        lock_identity(
            &mut transaction,
            record.target.kind,
            &record.target.resource_id,
        )
        .await?;
        ensure_unfenced(&mut transaction, &record.target).await?;
        let changed = sqlx::query(&format!(
            "INSERT INTO {NS}_references
             (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING"
        ))
        .bind(&record.target.workspace_id)
        .bind(kind_name(record.target.kind))
        .bind(&record.target.resource_id)
        .bind(reference_kind_name(record.reference.kind))
        .bind(&record.reference.reference_id)
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
        Ok(sqlx::query(&format!(
            "DELETE FROM {NS}_references
             WHERE workspace_id = $1 AND resource_kind = $2 AND resource_id = $3
               AND reference_kind = $4 AND reference_id = $5"
        ))
        .bind(&record.target.workspace_id)
        .bind(kind_name(record.target.kind))
        .bind(&record.target.resource_id)
        .bind(reference_kind_name(record.reference.kind))
        .bind(&record.reference.reference_id)
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
        validate_replacement(kind, reference_id, &records)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        let identities: BTreeSet<_> = records
            .iter()
            .map(|record| (record.target.kind, record.target.resource_id.as_str()))
            .collect();
        for (resource_kind, resource_id) in identities {
            lock_identity(&mut transaction, resource_kind, resource_id).await?;
            let target = records
                .iter()
                .find(|record| {
                    record.target.kind == resource_kind && record.target.resource_id == resource_id
                })
                .map(|record| &record.target)
                .expect("identity came from records");
            ensure_unfenced(&mut transaction, target).await?;
        }
        sqlx::query(&format!(
            "DELETE FROM {NS}_references WHERE reference_kind = $1 AND reference_id = $2"
        ))
        .bind(reference_kind_name(kind))
        .bind(reference_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage(error.to_string()))?;
        for record in records {
            sqlx::query(&format!(
                "INSERT INTO {NS}_references
                 (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                 VALUES ($1, $2, $3, $4, $5)"
            ))
            .bind(record.target.workspace_id)
            .bind(kind_name(record.target.kind))
            .bind(record.target.resource_id)
            .bind(reference_kind_name(record.reference.kind))
            .bind(record.reference.reference_id)
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
        let rows = sqlx::query(&format!(
            "SELECT reference_kind, reference_id FROM {NS}_references
             WHERE workspace_id = $1 AND resource_kind = $2 AND resource_id = $3
             ORDER BY reference_kind, reference_id"
        ))
        .bind(&target.workspace_id)
        .bind(kind_name(target.kind))
        .bind(&target.resource_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let kind: String = row.try_get("reference_kind").map_err(storage)?;
                Ok(ResourceReference {
                    kind: parse_reference_kind(&kind)?,
                    reference_id: row.try_get("reference_id").map_err(storage)?,
                })
            })
            .collect()
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage(error.to_string()))?;
        references_for_identity(&mut transaction, kind, resource_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(workspace: &str, resource_id: &str) -> ResourceReferenceRecord {
        ResourceReferenceRecord {
            target: ResourceTarget::new(workspace, ResourceKind::File, resource_id),
            reference: ResourceReference {
                kind: ResourceReferenceKind::WorkspaceOwnership,
                reference_id: format!("ownership-{workspace}"),
            },
        }
    }

    #[tokio::test]
    async fn live_postgres_serializes_references_with_reclamation_fences() {
        let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            return;
        };
        let store = PostgresResourceStore::connect(&url).await.unwrap();
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let resource_id = format!("resource-store-test-{suffix}");
        let intent_id = format!("intent-{suffix}");
        let a = reference("workspace-a", &resource_id);
        let b = reference("workspace-b", &resource_id);
        assert!(store.add_reference(a.clone()).await.unwrap());
        assert!(matches!(
            store
                .acquire_reclamation(&intent_id, &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Blocked(rows) if rows == vec![a.clone()]
        ));
        assert!(store.remove_reference(&a).await.unwrap());
        assert_eq!(
            store
                .acquire_reclamation(&intent_id, &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Acquired
        );
        assert!(matches!(
            store.add_reference(b.clone()).await,
            Err(ResourcePurgeError::ReclamationFenced { .. })
        ));
        assert!(
            store
                .release_reclamation(&intent_id, &a.target)
                .await
                .unwrap()
        );
        assert!(store.add_reference(b.clone()).await.unwrap());
        assert!(store.remove_reference(&b).await.unwrap());

        for ordinal in 0..16 {
            let race_id = format!("{resource_id}-race-{ordinal}");
            let race_intent = format!("{intent_id}-race-{ordinal}");
            let row = reference("workspace-race", &race_id);
            let (fence, reference_write) = tokio::join!(
                store.acquire_reclamation(&race_intent, &row.target),
                store.add_reference(row.clone())
            );
            match (fence.unwrap(), reference_write) {
                (
                    AcquireResourceReclamationOutcome::Acquired,
                    Err(ResourcePurgeError::ReclamationFenced { .. }),
                ) => {
                    assert!(
                        store
                            .release_reclamation(&race_intent, &row.target)
                            .await
                            .unwrap()
                    );
                }
                (AcquireResourceReclamationOutcome::Blocked(rows), Ok(true)) => {
                    assert_eq!(rows, vec![row.clone()]);
                    assert!(store.remove_reference(&row).await.unwrap());
                }
                other => panic!("reference/fence race admitted an invalid result: {other:?}"),
            }
        }
    }
}
