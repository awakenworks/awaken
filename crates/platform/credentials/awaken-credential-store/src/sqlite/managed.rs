// @formal entrypoint credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
// @formal entrypoint credential.managed_creation_begin_rejects_every_published_source_or_child_identity
// @formal entrypoint credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
// @formal entrypoint credential.managed_creation_publishes_source_and_child_atomically
// @formal entrypoint credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
// @formal entrypoint credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication
//! Transactional persistence for the Managed Credential aggregate.
//!
//! Complete decoded-row admission and BEGIN IMMEDIATE own snapshot CAS. Keyed
//! ownership and affected-row checks remain; JSON spelling is not a second fence.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

impl SqliteCredentialRepo {
    async fn record_rollout_delivery(
        &self,
        event: &ManagedCredentialRollout,
        delivered: bool,
    ) -> Result<(), CredentialError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let row = sqlx::query_as::<_, (String, bool)>(
            "SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=?1",
        )
        .bind(&event.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((data, was_delivered)) = row else {
            return if delivered {
                tx.commit().await.map_err(storage)
            } else {
                Err(CredentialError::MutationConflict(
                    "Managed credential redelivery has no exact receipt".into(),
                ))
            };
        };
        if delivered && was_delivered {
            return tx.commit().await.map_err(storage);
        }
        let durable: ManagedCredentialRollout = serde_json::from_str(&data).map_err(storage)?;
        if managed_credential_rollout_identity_decision(Some(&durable), event)
            != ManagedCredentialRolloutIdentityDecision::Exact
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential delivery receipt conflicts".into(),
            ));
        }
        if was_delivered != delivered {
            sqlx::query(
                "UPDATE credential_managed_credential_rollout SET delivered=?2 WHERE event_id=?1",
            )
            .bind(&event.id)
            .bind(delivered)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        if delivered && !was_delivered {
            let rows = sqlx::query_as::<_, (String, String)>("SELECT event_id,data FROM credential_managed_credential_rollout WHERE delivered=TRUE \
                AND (CASE WHEN json_valid(data) THEN json_extract(data,'$.workspace_id') END)=?1 \
                AND (CASE WHEN json_valid(data) THEN json_extract(data,'$.source_id') END)=?2")
            .bind(&event.workspace_id)
            .bind(&event.source_id.0)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?;
            for id in crate::rollout_receipts::obsolete_acknowledgements(rows) {
                sqlx::query("DELETE FROM credential_managed_credential_rollout WHERE event_id=?1 AND delivered=TRUE")
                    .bind(id).execute(&mut *tx).await.map_err(storage)?;
            }
        }
        tx.commit().await.map_err(storage)
    }
}

async fn publish_rollout(
    tx: &mut sqlx::SqliteConnection,
    rollout: &ManagedCredentialRollout,
) -> Result<(), ManagedCredentialMutationError> {
    sqlx::query("INSERT OR IGNORE INTO credential_managed_credential_rollout (event_id, data) VALUES (?1, ?2)")
    .bind(&rollout.id)
    .bind(serde_json::to_string(rollout).map_err(storage)?)
    .execute(&mut *tx)
    .await
    .map_err(storage)?;
    let (data, was_delivered) = sqlx::query_as::<_, (String, bool)>(
        "SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=?1",
    )
    .bind(&rollout.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    let durable: ManagedCredentialRollout = serde_json::from_str(&data).map_err(storage)?;
    if managed_credential_rollout_identity_decision(Some(&durable), rollout)
        == ManagedCredentialRolloutIdentityDecision::Collision
    {
        if !was_delivered {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        sqlx::query("UPDATE credential_managed_credential_rollout SET data=?2,delivered=FALSE WHERE event_id=?1")
        .bind(&rollout.id)
        .bind(serde_json::to_string(rollout).map_err(storage)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
    }
    Ok(())
}

fn invalid_pending(error: ManagedCredentialMutationError) -> CredentialError {
    match error {
        ManagedCredentialMutationError::Store(error) => error,
        error => CredentialError::MutationConflict(format!(
            "invalid durable Managed credential mutation: {error}"
        )),
    }
}

/// The one `data` column selected by a single text key, if the row exists.
async fn data_at(
    conn: &mut SqliteConnection,
    sql: &'static str,
    key: &str,
) -> Result<Option<String>, CredentialError> {
    sqlx::query_scalar(sql)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)
}

fn decode<T: serde::de::DeserializeOwned>(
    data: Option<String>,
) -> Result<Option<T>, CredentialError> {
    data.map(|data| serde_json::from_str(&data).map_err(storage))
        .transpose()
}

#[async_trait::async_trait]
impl ManagedCredentialRepository for SqliteCredentialRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
        now_unix_ms: u64,
    ) -> Result<bool, CredentialError> {
        pending
            .validate_for_begin(now_unix_ms)
            .map_err(invalid_pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable: Option<PendingManagedCredentialMutation> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?;
        if let Some(durable) = durable {
            if !durable.matches_logical_command(&pending) {
                return Err(CredentialError::MutationConflict(
                    "another Managed credential mutation is pending".into(),
                ));
            }
            tx.commit().await.map_err(storage)?;
            return Ok(false);
        }
        let current_source = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_source WHERE id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?;
        let current_child = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_vault_credential WHERE id = ?1",
                &pending.after_credential.id,
            )
            .await?,
        )?;
        if !pending.is_unpublished_at(current_source.as_ref(), current_child.as_ref()) {
            return Err(CredentialError::MutationConflict(
                "Managed credential changed before its mutation was prepared".into(),
            ));
        }
        let inserted = sqlx::query("INSERT OR IGNORE INTO credential_managed_credential_mutation (source_id, data) VALUES (?1, ?2)")
        .bind(&pending.after_source.id.0)
        .bind(serde_json::to_string(&pending).map_err(storage)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        let durable = data_at(
            &mut tx,
            "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
            &pending.after_source.id.0,
        )
        .await?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential mutation lost its pending fact".into(),
            )
        })?;
        let durable: PendingManagedCredentialMutation =
            serde_json::from_str(&durable).map_err(storage)?;
        if !durable.matches_logical_command(&pending) {
            return Err(CredentialError::MutationConflict(
                "another Managed credential mutation is pending".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(inserted == 1)
    }

    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
        pending.validate()?;
        let plan = managed_credential_commit_plan(pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable: PendingManagedCredentialMutation = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential mutation has no durable pending fact".into(),
            )
        })?;
        durable.validate()?;
        if durable.material_fence.phase != CredentialMaterialMutationPhase::Reclaiming
            && (&durable != pending
                || pending.material_fence.phase != CredentialMaterialMutationPhase::Ready)
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential mutation is not ready or does not match its durable fact"
                    .into(),
            )
            .into());
        }
        let vault: Option<ManagedVault> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_vault WHERE id = ?1",
                &pending.after_credential.vault_id,
            )
            .await?,
        )?;
        let current_source: Option<CredentialSource> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_source WHERE id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?;
        let current_child: Option<ManagedVaultCredential> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_vault_credential WHERE id = ?1",
                &pending.after_credential.id,
            )
            .await?,
        )?;
        if durable.material_fence.phase == CredentialMaterialMutationPhase::Reclaiming {
            if durable != plan.reclaiming_mutation
                || current_source.as_ref() != Some(&plan.source)
                || current_child.as_ref() != Some(&plan.credential)
            {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            tx.commit().await.map_err(storage)?;
            return Ok(durable);
        }
        if !pending.is_unpublished_at(current_source.as_ref(), current_child.as_ref()) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT data FROM credential_managed_vault_credential \
             WHERE workspace_id = ?1 AND vault_id = ?2 AND id <> ?3 ORDER BY id",
        )
        .bind(&pending.after_credential.workspace_id)
        .bind(&pending.after_credential.vault_id)
        .bind(&pending.after_credential.id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|data| serde_json::from_str(&data).map_err(storage))
        .collect::<Result<Vec<ManagedVaultCredential>, _>>()?;
        match pending.operation {
            ManagedCredentialOperation::Create => admit_managed_credential_insert(
                &pending.after_credential.workspace_id,
                vault.as_ref(),
                &existing,
                &pending.after_credential,
            )?,
            ManagedCredentialOperation::Update => {
                let before = pending
                    .before_credential
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    Some(before),
                    before.revision,
                    &pending.after_credential,
                )?;
                let mut admission = pending.after_credential.clone();
                admission.revision = 1;
                admission.lifecycle = ManagedCredentialLifecycle::Active;
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault.as_ref(),
                    &existing,
                    &admission,
                )?;
            }
            ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete => {
                let before = pending
                    .before_credential
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    Some(before),
                    before.revision,
                    &pending.after_credential,
                )?;
                if !managed_retirement_parent_admitted(
                    pending.operation,
                    vault.as_ref(),
                    &pending.after_credential,
                ) {
                    return Err(if vault.is_some() {
                        ManagedCredentialMutationError::InvalidLifecycle
                    } else {
                        ManagedCredentialMutationError::NotFound
                    });
                }
            }
        }
        let source_changed = if pending.operation == ManagedCredentialOperation::Create {
            sqlx::query("INSERT OR IGNORE INTO credential_source (id, workspace_id, data) VALUES (?1, ?2, ?3)")
            .bind(&pending.after_source.id.0)
            .bind(&pending.after_source.workspace_id)
            .bind(serde_json::to_string(&plan.source).map_err(storage)?)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        } else {
            let before = pending
                .before_source
                .as_ref()
                .ok_or(ManagedCredentialMutationError::NotFound)?;
            sqlx::query(
                "UPDATE credential_source SET workspace_id = ?1, data = ?2 \
                 WHERE id = ?3 AND workspace_id = ?4",
            )
            .bind(&pending.after_source.workspace_id)
            .bind(serde_json::to_string(&plan.source).map_err(storage)?)
            .bind(&pending.after_source.id.0)
            .bind(&before.workspace_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        };
        if source_changed != 1 {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        let child_changed = if pending.operation == ManagedCredentialOperation::Create {
            sqlx::query(
                "INSERT OR IGNORE INTO credential_managed_vault_credential \
                 (id, vault_id, workspace_id, source_id, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&pending.after_credential.id)
            .bind(&pending.after_credential.vault_id)
            .bind(&pending.after_credential.workspace_id)
            .bind(&pending.after_credential.source_id.0)
            .bind(serde_json::to_string(&plan.credential).map_err(storage)?)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        } else {
            let before = pending
                .before_credential
                .as_ref()
                .ok_or(ManagedCredentialMutationError::NotFound)?;
            sqlx::query(
                "UPDATE credential_managed_vault_credential SET vault_id = ?1, \
                 workspace_id = ?2, source_id = ?3, data = ?4 WHERE id = ?5 \
                 AND vault_id = ?6 AND workspace_id = ?7 AND source_id = ?8",
            )
            .bind(&pending.after_credential.vault_id)
            .bind(&pending.after_credential.workspace_id)
            .bind(&pending.after_credential.source_id.0)
            .bind(serde_json::to_string(&plan.credential).map_err(storage)?)
            .bind(&pending.after_credential.id)
            .bind(&before.vault_id)
            .bind(&before.workspace_id)
            .bind(&before.source_id.0)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        };
        if child_changed != 1 {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        sqlx::query(
            "UPDATE credential_managed_credential_mutation SET data = ?1 WHERE source_id = ?2",
        )
        .bind(serde_json::to_string(&plan.reclaiming_mutation).map_err(storage)?)
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(rollout) = plan.rollout.as_ref() {
            publish_rollout(&mut tx, rollout).await?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(plan.reclaiming_mutation)
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let data = data_at(
            &mut tx,
            "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
            &pending.after_source.id.0,
        )
        .await?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential mutation has no durable pending fact".into(),
            )
        })?;
        let durable: PendingManagedCredentialMutation =
            serde_json::from_str(&data).map_err(storage)?;
        if &durable != pending
            || pending.material_fence.phase != CredentialMaterialMutationPhase::Writing
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential ready transition does not match Writing".into(),
            ));
        }
        let ready = durable.with_material_ready()?;
        sqlx::query(
            "UPDATE credential_managed_credential_mutation SET data = ?1 WHERE source_id = ?2",
        )
        .bind(serde_json::to_string(&ready).map_err(storage)?)
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(ready)
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT source_id, data FROM credential_managed_credential_mutation \
             ORDER BY created_at, source_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(source_id, data)| {
                let pending = serde_json::from_str::<PendingManagedCredentialMutation>(&data)
                    .map_err(storage)?;
                pending.validate_durable_key(&source_id)?;
                Ok(pending)
            })
            .collect()
    }

    async fn claim_expired_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<PendingManagedCredentialMutation>, CredentialError> {
        let Some(claimed) = pending.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        claimed.validate().map_err(invalid_pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current = data_at(
            &mut tx,
            "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
            &pending.after_source.id.0,
        )
        .await?;
        let Some(current) = current else {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        };
        let durable: PendingManagedCredentialMutation =
            serde_json::from_str(&current).map_err(storage)?;
        // Successful claim already requires Writing; full snapshot equality
        // also fences phase changes before this locked write.
        if &durable != pending {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        }
        let changed = sqlx::query(
            "UPDATE credential_managed_credential_mutation SET data = ?1 \
                 WHERE source_id = ?2",
        )
        .bind(serde_json::to_string(&claimed).map_err(storage)?)
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Ok(None);
        }
        tx.commit().await.map_err(storage)?;
        Ok(Some(claimed))
    }

    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current_source: Option<CredentialSource> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_source WHERE id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?;
        let current_child: Option<ManagedVaultCredential> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_vault_credential WHERE id = ?1",
                &pending.after_credential.id,
            )
            .await?,
        )?;
        let current = data_at(
            &mut tx,
            "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
            &pending.after_source.id.0,
        )
        .await?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential abort has no durable pending fact".into(),
            )
        })?;
        let durable: PendingManagedCredentialMutation =
            serde_json::from_str(&current).map_err(storage)?;
        let reclaiming =
            pending.admit_abort(&durable, current_source.as_ref(), current_child.as_ref())?;
        let changed = sqlx::query(
            "UPDATE credential_managed_credential_mutation SET data = ?1 \
                 WHERE source_id = ?2",
        )
        .bind(serde_json::to_string(&reclaiming).map_err(storage)?)
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Err(CredentialError::MutationConflict(
                "Managed credential abort lost its exact durable fact".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(reclaiming)
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current = data_at(
            &mut tx,
            "SELECT data FROM credential_managed_credential_mutation WHERE source_id = ?1",
            &pending.after_source.id.0,
        )
        .await?;
        let Some(current) = current else {
            tx.commit().await.map_err(storage)?;
            return Ok(());
        };
        let durable: PendingManagedCredentialMutation =
            serde_json::from_str(&current).map_err(storage)?;
        let current_source: Option<CredentialSource> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_source WHERE id = ?1",
                &pending.after_source.id.0,
            )
            .await?,
        )?;
        let current_child: Option<ManagedVaultCredential> = decode(
            data_at(
                &mut tx,
                "SELECT data FROM credential_managed_vault_credential WHERE id = ?1",
                &pending.after_credential.id,
            )
            .await?,
        )?;
        pending.admit_completion(&durable, current_source.as_ref(), current_child.as_ref())?;
        let changed = sqlx::query(
            "DELETE FROM credential_managed_credential_mutation \
                 WHERE source_id = ?1",
        )
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Err(CredentialError::MutationConflict(
                "Managed credential completion lost its exact durable fact".into(),
            ));
        }
        tx.commit().await.map_err(storage)
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        decode(
            data_at(
                &mut conn,
                "SELECT data FROM credential_managed_credential_rollout WHERE event_id = ?1",
                event_id,
            )
            .await?,
        )
    }

    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError> {
        let rows = sqlx::query_as::<_, (String, String)>("SELECT event_id, data FROM credential_managed_credential_rollout WHERE delivered=FALSE \
             ORDER BY created_at, event_id")
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut rollouts = Vec::new();
        for (event_id, data) in rows {
            match serde_json::from_str(&data) {
                Ok(value) => rollouts.push(value),
                Err(error) => tracing::error!(
                    recovery_record = "managed_credential_rollout",
                    record_id = %event_id,
                    error = %error,
                    "retaining and isolating an undecodable durable recovery record"
                ),
            }
        }
        Ok(rollouts)
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        self.record_rollout_delivery(rollout, true).await
    }

    async fn rearm_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        self.record_rollout_delivery(rollout, false).await
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT data FROM credential_managed_vault ORDER BY workspace_id, id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut pending = Vec::new();
        for row in rows {
            let vault: ManagedVault = serde_json::from_str(&row).map_err(storage)?;
            if vault.deletion_requested() {
                pending.push(vault);
            }
        }
        Ok(pending)
    }
}
