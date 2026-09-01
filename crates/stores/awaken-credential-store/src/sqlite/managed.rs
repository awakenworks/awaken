//! Transactional persistence for the Managed Credential aggregate.

use super::*;

fn invalid_pending(error: ManagedCredentialMutationError) -> CredentialError {
    match error {
        ManagedCredentialMutationError::Store(error) => error,
        error => CredentialError::MutationConflict(format!(
            "invalid durable Managed credential mutation: {error}"
        )),
    }
}

async fn with_conn_managed_mutation<T, F>(
    conn: &awaken_sqlite_runtime::SharedSqliteConnection,
    f: F,
) -> Result<T, ManagedCredentialMutationError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection, &str) -> Result<T, ManagedCredentialMutationError> + Send + 'static,
{
    awaken_sqlite_runtime::with_connection(conn.clone(), move |connection| f(connection, NS))
        .await
        .map_err(|error| ManagedCredentialMutationError::Store(storage(error)))?
}

#[async_trait::async_trait]
impl ManagedCredentialRepository for SqliteCredentialRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<bool, CredentialError> {
        pending.validate_for_begin().map_err(invalid_pending)?;
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let durable: Option<PendingManagedCredentialMutation> = tx
                .query_row(
                    &format!(
                        "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"
                    ),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            if let Some(durable) = durable {
                if !durable.matches_logical_command(&pending) {
                    return Err(CredentialError::MutationConflict(
                        "another Managed credential mutation is pending".into(),
                    ));
                }
                tx.commit().map_err(storage)?;
                return Ok(false);
            }
            let current_source = tx
                .query_row(
                    &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let current_child = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_vault_credential WHERE id = ?1"),
                    params![pending.after_credential.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            if current_source != pending.before_source || current_child != pending.before_credential
            {
                return Err(CredentialError::MutationConflict(
                    "Managed credential changed before its mutation was prepared".into(),
                ));
            }
            let inserted = tx.execute(
                &format!("INSERT OR IGNORE INTO {p}_managed_credential_mutation (source_id, data) VALUES (?1, ?2)"),
                params![pending.after_source.id.0, serde_json::to_string(&pending).map_err(storage)?],
            )
            .map_err(storage)?;
            let durable = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| CredentialError::MutationConflict(
                    "Managed credential mutation lost its pending fact".into()
                ))?;
            let durable: PendingManagedCredentialMutation =
                serde_json::from_str(&durable).map_err(storage)?;
            if !durable.matches_logical_command(&pending) {
                return Err(CredentialError::MutationConflict(
                    "another Managed credential mutation is pending".into(),
                ));
            }
            tx.commit().map_err(storage)?;
            Ok(inserted == 1)
        })
        .await
    }

    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
        pending.validate()?;
        let pending = pending.clone();
        with_conn_managed_mutation(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let durable: PendingManagedCredentialMutation = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?
                .ok_or_else(|| CredentialError::MutationConflict(
                    "Managed credential mutation has no durable pending fact".into()
                ))?;
            durable.validate()?;
            if durable.material_fence.phase != CredentialMaterialMutationPhase::Reclaiming
                && (durable != pending
                    || pending.material_fence.phase != CredentialMaterialMutationPhase::Ready)
            {
                return Err(CredentialError::MutationConflict(
                    "Managed credential mutation is not ready or does not match its durable fact".into(),
                ).into());
            }
            let vault = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_vault WHERE id = ?1"),
                    params![pending.after_credential.vault_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let current_source = tx
                .query_row(
                    &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let current_child = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_vault_credential WHERE id = ?1"),
                    params![pending.after_credential.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            if durable.material_fence.phase == CredentialMaterialMutationPhase::Reclaiming {
                let expected_reclaiming = pending.with_material_reclaiming()?;
                if durable != expected_reclaiming
                    || current_source.as_ref() != Some(&pending.after_source)
                    || current_child.as_ref() != Some(&pending.after_credential)
                {
                    return Err(ManagedCredentialMutationError::RevisionConflict);
                }
                tx.commit().map_err(storage)?;
                return Ok(durable);
            }
            if current_source != pending.before_source || current_child != pending.before_credential
            {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            let mut statement = tx
                .prepare(&format!(
                    "SELECT data FROM {p}_managed_vault_credential \
                     WHERE workspace_id = ?1 AND vault_id = ?2 AND id <> ?3 ORDER BY id"
                ))
                .map_err(storage)?;
            let existing = statement
                .query_map(
                    params![
                        pending.after_credential.workspace_id,
                        pending.after_credential.vault_id,
                        pending.after_credential.id
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(storage)?
                .map(|row| {
                    row.map_err(storage)
                        .and_then(|data| serde_json::from_str(&data).map_err(storage))
                })
                .collect::<Result<Vec<ManagedVaultCredential>, _>>()?;
            drop(statement);
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
                tx.execute(
                    &format!("INSERT OR IGNORE INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3)"),
                    params![
                        pending.after_source.id.0,
                        pending.after_source.workspace_id,
                        serde_json::to_string(&pending.after_source).map_err(storage)?
                    ],
                )
                .map_err(storage)?
            } else {
                let before = pending
                    .before_source
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                tx.execute(
                    &format!("UPDATE {p}_source SET workspace_id = ?1, data = ?2 \
                             WHERE id = ?3 AND workspace_id = ?4 AND data = ?5"),
                    params![
                        pending.after_source.workspace_id,
                        serde_json::to_string(&pending.after_source).map_err(storage)?,
                        pending.after_source.id.0,
                        before.workspace_id,
                        serde_json::to_string(before).map_err(storage)?
                    ],
                )
                .map_err(storage)?
            };
            if source_changed != 1 {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            let child_changed = if pending.operation == ManagedCredentialOperation::Create {
                tx.execute(
                    &format!("INSERT OR IGNORE INTO {p}_managed_vault_credential \
                             (id, vault_id, workspace_id, source_id, data) VALUES (?1, ?2, ?3, ?4, ?5)"),
                    params![
                        pending.after_credential.id,
                        pending.after_credential.vault_id,
                        pending.after_credential.workspace_id,
                        pending.after_credential.source_id.0,
                        serde_json::to_string(&pending.after_credential).map_err(storage)?
                    ],
                )
                .map_err(storage)?
            } else {
                let before = pending
                    .before_credential
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                tx.execute(
                    &format!("UPDATE {p}_managed_vault_credential SET vault_id = ?1, \
                             workspace_id = ?2, source_id = ?3, data = ?4 WHERE id = ?5 \
                             AND vault_id = ?6 AND workspace_id = ?7 AND source_id = ?8 AND data = ?9"),
                    params![
                        pending.after_credential.vault_id,
                        pending.after_credential.workspace_id,
                        pending.after_credential.source_id.0,
                        serde_json::to_string(&pending.after_credential).map_err(storage)?,
                        pending.after_credential.id,
                        before.vault_id,
                        before.workspace_id,
                        before.source_id.0,
                        serde_json::to_string(before).map_err(storage)?
                    ],
                )
                .map_err(storage)?
            };
            if child_changed != 1 {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            let reclaiming = pending
                .with_material_reclaiming()
                .map_err(ManagedCredentialMutationError::Store)?;
            tx.execute(
                &format!("UPDATE {p}_managed_credential_mutation SET data = ?1 WHERE source_id = ?2"),
                params![
                    serde_json::to_string(&reclaiming).map_err(storage)?,
                    pending.after_source.id.0
                ],
            )
            .map_err(storage)?;
            if let Some(rollout) = managed_rollout_from_committed(&pending) {
                tx.execute(
                    &format!("INSERT OR IGNORE INTO {p}_managed_credential_rollout (event_id, data) VALUES (?1, ?2)"),
                    params![rollout.id, serde_json::to_string(&rollout).map_err(storage)?],
                )
                .map_err(storage)?;
                let durable = tx
                    .query_row(
                        &format!("SELECT data FROM {p}_managed_credential_rollout WHERE event_id = ?1"),
                        params![rollout.id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(storage)?;
                let durable: ManagedCredentialRollout =
                    serde_json::from_str(&durable).map_err(storage)?;
                if durable != rollout {
                    return Err(ManagedCredentialMutationError::RevisionConflict);
                }
            }
            tx.commit().map_err(storage)?;
            Ok(reclaiming)
        })
        .await
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let pending = pending.clone();
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let data = tx
                .query_row(
                    &format!(
                        "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"
                    ),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "Managed credential mutation has no durable pending fact".into(),
                    )
                })?;
            let durable: PendingManagedCredentialMutation =
                serde_json::from_str(&data).map_err(storage)?;
            if durable != pending
                || pending.material_fence.phase != CredentialMaterialMutationPhase::Writing
            {
                return Err(CredentialError::MutationConflict(
                    "Managed credential ready transition does not match Writing".into(),
                ));
            }
            let ready = durable.with_material_ready()?;
            tx.execute(
                &format!(
                    "UPDATE {p}_managed_credential_mutation SET data = ?1 WHERE source_id = ?2"
                ),
                params![
                    serde_json::to_string(&ready).map_err(storage)?,
                    pending.after_source.id.0
                ],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
            Ok(ready)
        })
        .await
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        with_conn(&self.conn, move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT source_id, data FROM {p}_managed_credential_mutation \
                     ORDER BY created_at, source_id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage)?;
            rows.map(|row| {
                let (source_id, data) = row.map_err(storage)?;
                let pending = serde_json::from_str::<PendingManagedCredentialMutation>(&data)
                    .map_err(storage)?;
                pending.validate_durable_key(&source_id)?;
                Ok(pending)
            })
            .collect()
        })
        .await
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
        let pending = pending.clone();
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current = tx
                .query_row(
                    &format!(
                        "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"
                    ),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?;
            let Some(current) = current else {
                tx.commit().map_err(storage)?;
                return Ok(None);
            };
            let durable: PendingManagedCredentialMutation =
                serde_json::from_str(&current).map_err(storage)?;
            if durable != pending
                || durable.material_fence.phase != CredentialMaterialMutationPhase::Writing
            {
                tx.commit().map_err(storage)?;
                return Ok(None);
            }
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE {p}_managed_credential_mutation SET data = ?1 \
                             WHERE source_id = ?2 AND data = ?3"
                    ),
                    params![
                        serde_json::to_string(&claimed).map_err(storage)?,
                        pending.after_source.id.0,
                        current
                    ],
                )
                .map_err(storage)?;
            if changed != 1 {
                return Ok(None);
            }
            tx.commit().map_err(storage)?;
            Ok(Some(claimed))
        })
        .await
    }

    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let pending = pending.clone();
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current_source = tx
                .query_row(
                    &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let current_child = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_vault_credential WHERE id = ?1"),
                    params![pending.after_credential.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            if current_source != pending.before_source || current_child != pending.before_credential
            {
                return Err(CredentialError::MutationConflict(
                    "cannot abort a published or superseded Managed credential mutation".into(),
                ));
            }
            let current = tx
                .query_row(
                    &format!(
                        "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"
                    ),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "Managed credential abort has no durable pending fact".into(),
                    )
                })?;
            let durable: PendingManagedCredentialMutation =
                serde_json::from_str(&current).map_err(storage)?;
            if durable == pending
                && pending.material_fence.phase == CredentialMaterialMutationPhase::ReclaimingAbort
            {
                tx.commit().map_err(storage)?;
                return Ok(durable);
            }
            if durable != pending
                || !matches!(
                    pending.material_fence.phase,
                    CredentialMaterialMutationPhase::Writing
                        | CredentialMaterialMutationPhase::Ready
                )
            {
                return Err(CredentialError::MutationConflict(
                    "Managed credential abort does not match its durable pending fact".into(),
                ));
            }
            let reclaiming = pending.with_material_reclaiming_abort()?;
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE {p}_managed_credential_mutation SET data = ?1 \
                             WHERE source_id = ?2 AND data = ?3"
                    ),
                    params![
                        serde_json::to_string(&reclaiming).map_err(storage)?,
                        pending.after_source.id.0,
                        current
                    ],
                )
                .map_err(storage)?;
            if changed != 1 {
                return Err(CredentialError::MutationConflict(
                    "Managed credential abort lost its exact durable fact".into(),
                ));
            }
            tx.commit().map_err(storage)?;
            Ok(reclaiming)
        })
        .await
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending.validate().map_err(invalid_pending)?;
        let pending = pending.clone();
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current = tx
                .query_row(
                    &format!(
                        "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = ?1"
                    ),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?;
            let Some(current) = current else {
                tx.commit().map_err(storage)?;
                return Ok(());
            };
            let durable: PendingManagedCredentialMutation =
                serde_json::from_str(&current).map_err(storage)?;
            let current_source = tx
                .query_row(
                    &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                    params![pending.after_source.id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let current_child = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_vault_credential WHERE id = ?1"),
                    params![pending.after_credential.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            let truth_matches_phase = match pending.material_fence.phase {
                CredentialMaterialMutationPhase::Reclaiming => {
                    current_source.as_ref() == Some(&pending.after_source)
                        && current_child.as_ref() == Some(&pending.after_credential)
                }
                CredentialMaterialMutationPhase::ReclaimingAbort => {
                    current_source == pending.before_source
                        && current_child == pending.before_credential
                }
                CredentialMaterialMutationPhase::Writing
                | CredentialMaterialMutationPhase::Ready => false,
            };
            if durable != pending || !truth_matches_phase {
                return Err(CredentialError::MutationConflict(
                    "Managed credential completion does not match its durable cleanup truth".into(),
                ));
            }
            let changed = tx
                .execute(
                    &format!(
                        "DELETE FROM {p}_managed_credential_mutation \
                             WHERE source_id = ?1 AND data = ?2"
                    ),
                    params![pending.after_source.id.0, current],
                )
                .map_err(storage)?;
            if changed != 1 {
                return Err(CredentialError::MutationConflict(
                    "Managed credential completion lost its exact durable fact".into(),
                ));
            }
            tx.commit().map_err(storage)
        })
        .await
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        let event_id = event_id.to_owned();
        with_conn(&self.conn, move |conn, p| {
            conn.query_row(
                &format!("SELECT data FROM {p}_managed_credential_rollout WHERE event_id = ?1"),
                params![event_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
        })
        .await
    }

    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError> {
        with_conn(&self.conn, |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT event_id, data FROM {p}_managed_credential_rollout \
                     ORDER BY created_at, event_id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage)?;
            let mut rollouts = Vec::new();
            for row in rows {
                let (event_id, data) = row.map_err(storage)?;
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
        })
        .await
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let rollout = rollout.clone();
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current = tx
                .query_row(
                    &format!("SELECT data FROM {p}_managed_credential_rollout WHERE event_id = ?1"),
                    params![rollout.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?;
            let Some(current) = current else {
                tx.commit().map_err(storage)?;
                return Ok(());
            };
            let durable: ManagedCredentialRollout =
                serde_json::from_str(&current).map_err(storage)?;
            if durable != rollout {
                return Err(CredentialError::MutationConflict(
                    "Managed credential rollout acknowledgement is stale".into(),
                ));
            }
            let changed = tx
                .execute(
                    &format!(
                        "DELETE FROM {p}_managed_credential_rollout \
                             WHERE event_id = ?1 AND data = ?2"
                    ),
                    params![rollout.id, current],
                )
                .map_err(storage)?;
            if changed != 1 {
                return Err(CredentialError::MutationConflict(
                    "Managed credential rollout acknowledgement lost its exact event".into(),
                ));
            }
            tx.commit().map_err(storage)
        })
        .await
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        with_conn(&self.conn, move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT data FROM {p}_managed_vault ORDER BY workspace_id, id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            let mut pending = Vec::new();
            for row in rows {
                let vault: ManagedVault =
                    serde_json::from_str(&row.map_err(storage)?).map_err(storage)?;
                if vault.deletion_requested() {
                    pending.push(vault);
                }
            }
            Ok(pending)
        })
        .await
    }
}
