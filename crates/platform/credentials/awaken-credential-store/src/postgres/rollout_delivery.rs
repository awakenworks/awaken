// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: PostgreSQL persistence of the existing Credential outbox's
// @formal reason: exact delivery/rearm decision, serialized by source lineage.
// @formal reason: The shared three-backend conformance suite owns its regression
// @formal reason: matrix; this storage adapter adds no independent domain authority.
//! Transactional delivery receipts of the one Managed Credential outbox.

use super::{PostgresCredentialRepo, storage};
use awaken_credential_vault::repo::{
    ManagedCredentialRollout, ManagedCredentialRolloutIdentityDecision,
    managed_credential_rollout_identity_decision,
};
use awaken_credential_vault::{CredentialError, catalog::ManagedCredentialMutationError};
use sqlx::{Row, types::Json};

impl PostgresCredentialRepo {
    pub(super) async fn record_rollout_delivery(
        &self,
        event: &ManagedCredentialRollout,
        delivered: bool,
    ) -> Result<(), CredentialError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // The source row may have been purged. Serialize this receipt lineage
        // even then, without widening the lock to other sources.
        let lineage = format!(
            "awaken.credential.rollout:{}:{}:{}:{}",
            event.workspace_id.len(),
            event.workspace_id,
            event.source_id.0.len(),
            event.source_id.0
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(lineage)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        // Reuse the source row's existing transaction fence before event locks.
        // Two acknowledgements of this source cannot lock their events in
        // opposite order while pruning; publication uses the same source-first
        // ordering. The lineage lock also covers already-purged sources.
        sqlx::query("SELECT id FROM credential_source WHERE id=$1 FOR UPDATE")
            .bind(&event.source_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        let row = sqlx::query("SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=$1 FOR UPDATE")
            .bind(&event.id).fetch_optional(&mut *tx).await.map_err(storage)?;
        let Some(row) = row else {
            return if delivered {
                tx.commit().await.map_err(storage)
            } else {
                Err(CredentialError::MutationConflict(
                    "Managed credential redelivery has no exact receipt".into(),
                ))
            };
        };
        let was_delivered: bool = row.try_get("delivered").map_err(storage)?;
        if delivered && was_delivered {
            return tx.commit().await.map_err(storage);
        }
        let Json(durable): Json<ManagedCredentialRollout> = row.try_get("data").map_err(storage)?;
        if managed_credential_rollout_identity_decision(Some(&durable), event)
            != ManagedCredentialRolloutIdentityDecision::Exact
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential delivery receipt conflicts".into(),
            ));
        }
        if was_delivered != delivered {
            sqlx::query(
                "UPDATE credential_managed_credential_rollout SET delivered=$2 WHERE event_id=$1",
            )
            .bind(&event.id)
            .bind(delivered)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        if delivered && !was_delivered {
            let rows = sqlx::query_as::<_, (String, String)>("SELECT event_id,data::text FROM credential_managed_credential_rollout WHERE delivered=TRUE AND data->>'workspace_id'=$1 AND data->>'source_id'=$2 FOR UPDATE")
                .bind(&event.workspace_id).bind(&event.source_id.0).fetch_all(&mut *tx).await.map_err(storage)?;
            for id in crate::rollout_receipts::obsolete_acknowledgements(rows) {
                sqlx::query("DELETE FROM credential_managed_credential_rollout WHERE event_id=$1 AND delivered=TRUE")
                    .bind(id).execute(&mut *tx).await.map_err(storage)?;
            }
        }
        tx.commit().await.map_err(storage)
    }
}

// Called inside the pair's existing transaction after source/child CAS admission.
pub(super) async fn publish_rollout(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rollout: &ManagedCredentialRollout,
) -> Result<(), ManagedCredentialMutationError> {
    sqlx::query(
        "INSERT INTO credential_managed_credential_rollout (event_id, data) VALUES ($1, $2) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(&rollout.id)
    .bind(Json(rollout))
    .execute(&mut **tx)
    .await
    .map_err(storage)?;
    let row = sqlx::query("SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=$1 FOR UPDATE")
    .bind(&rollout.id)
    .fetch_one(&mut **tx)
    .await
    .map_err(storage)?;
    let was_delivered: bool = row.try_get("delivered").map_err(storage)?;
    let Json(durable): Json<ManagedCredentialRollout> = row.try_get("data").map_err(storage)?;
    if managed_credential_rollout_identity_decision(Some(&durable), rollout)
        == ManagedCredentialRolloutIdentityDecision::Collision
    {
        if !was_delivered {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        // An acknowledged key was previously absent/reusable. Retain
        // that admission contract without replacing a pending fact.
        sqlx::query("UPDATE credential_managed_credential_rollout SET data=$2,delivered=FALSE WHERE event_id=$1")
        .bind(&rollout.id)
        .bind(Json(rollout))
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    }
    Ok(())
}
