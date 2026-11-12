// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: The original Session mutation and receipt SQL shares the caller's transaction;
// @formal reason: adapter conformance covers this boundary, not an independent durability proof.
// The original Session root SQL, with caller-owned or independently owned commit.
// No admitted claim or ambient transaction is stored in this adapter.
use super::{
    EncodedSessionRow, PostgresManagedSessionRepository, Row, SessionCommandReceiptKind,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRepositoryError,
    SessionRevision, aggregate_str, corrupt, db_revision, event_admissions,
    insert_session_revision, lifecycle_str, receipt_kind_value, replay, storage,
    validate_current_mutation, write_not_committed,
};

impl PostgresManagedSessionRepository {
    /// Stage the original validated Session mutation on a caller's transaction.
    ///
    /// This method neither acquires a pool connection nor commits/rolls back.
    /// Its result is not a durable receipt until the caller commits. In
    /// particular an error carries no `write_not_committed` certificate: only
    /// the outer transaction owner knows its final rollback/unknown outcome.
    pub async fn commit_on(
        connection: &mut sqlx::PgConnection,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let next = mutation
            .validate()
            .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
        Self::commit_validated_on(connection, owner_scope, mutation, next)
            .await
            .map(|(result, _)| result)
    }

    pub(super) async fn commit_owned(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let mut commit_outcome_unknown = false;
        async {
            // Preserve the original pre-check before checkout.
            let next = mutation
                .validate()
                .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
            let mut transaction = self.pool.begin().await.map_err(storage)?;
            let (result, writes) =
                Self::commit_validated_on(&mut transaction, owner_scope, mutation, next).await?;
            if !writes {
                // Exact replay/conflict retains the original rollback-on-drop.
                return Ok(result);
            }
            let committed = transaction.commit().await;
            commit_outcome_unknown = !matches!(&committed, Ok(()) | Err(sqlx::Error::Database(_)));
            committed.map_err(storage)?;
            Ok(result)
        }
        .await
        .map_err(|error| {
            if commit_outcome_unknown {
                error
            } else {
                write_not_committed(error)
            }
        })
    }

    async fn commit_validated_on(
        connection: &mut sqlx::PgConnection,
        owner_scope: &str,
        mutation: SessionMutation,
        next: SessionRevision,
    ) -> Result<(SessionMutationResult, bool), SessionRepositoryError> {
        let receipt_kind = mutation.receipt_kind();
        let session_id = mutation.payload.session_id().to_string();
        if let Some(replay) =
            Self::mutation_replay(&mut *connection, owner_scope, &mutation, next).await?
        {
            return Ok((replay, false));
        }
        let current = sqlx::query(
            "SELECT revision, scope_id, aggregate_json
         FROM managed_session WHERE session_id = $1 FOR UPDATE",
        )
        .bind(&session_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage)?;
        let Some(current) = current else {
            let tombstone = sqlx::query(
                "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = $1",
            )
            .bind(&session_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage)?;
            let revision = tombstone.map_or(0, |row| row.get::<i64, _>("deleted_revision"));
            let revision = u64::try_from(revision)
                .map_err(|_| corrupt("negative deleted Session revision"))?;
            return Ok((
                SessionMutationResult::Conflict {
                    current_revision: SessionRevision(revision),
                },
                false,
            ));
        };
        let current_revision = SessionRevision(
            u64::try_from(current.get::<i64, _>("revision"))
                .map_err(|_| corrupt("negative managed Session revision"))?,
        );
        let current_owner: String = current.get("scope_id");
        if current_owner != owner_scope || current_revision != mutation.expected_revision {
            return Ok((SessionMutationResult::Conflict { current_revision }, false));
        }
        if let Some(replay) =
            Self::mutation_replay(&mut *connection, owner_scope, &mutation, next).await?
        {
            return Ok((replay, false));
        }
        let (mut deleted_admissions, private_content) = validate_current_mutation(
            &mutation,
            EncodedSessionRow {
                aggregate_json: current.try_get("aggregate_json").map_err(storage)?,
                revision: current.try_get("revision").map_err(storage)?,
            },
        )?;
        if matches!(&mutation.payload, SessionMutationPayload::Delete(_)) {
            let noops = sqlx::query_as::<_, (String, String, i64)>(
                "SELECT idempotency_key, payload_hash, committed_revision \
                 FROM managed_session_idempotency \
                 WHERE session_id = $1 AND receipt_kind = $2",
            )
            .bind(&session_id)
            .bind(receipt_kind_value(SessionCommandReceiptKind::EventNoOp))
            .fetch_all(&mut *connection)
            .await
            .map_err(storage)?;
            if !noops.is_empty() {
                deleted_admissions = Some(event_admissions::merge_noops(
                    deleted_admissions
                        .as_deref()
                        .expect("delete encoded its root receipts"),
                    current_revision,
                    noops,
                )?);
            }
        }
        match &mutation.payload {
            SessionMutationPayload::Replace(replacement) => {
                let mut replacement = replacement.clone();
                replacement.revision = next;
                let affected = sqlx::query(
                    "UPDATE managed_session SET
                    aggregate_json = $2, revision = $3
                 WHERE session_id = $1 AND scope_id = $4 AND revision = $5",
                )
                .bind(replacement.session_id.as_str())
                .bind(aggregate_str(&replacement)?)
                .bind(db_revision(next)?)
                .bind(owner_scope)
                .bind(db_revision(current_revision)?)
                .execute(&mut *connection)
                .await
                .map_err(storage)?
                .rows_affected();
                if affected != 1 {
                    return Ok((SessionMutationResult::Conflict { current_revision }, false));
                }
                Self::sync_session_indexes(&mut *connection, &replacement).await?;
                insert_session_revision(&mut *connection, owner_scope, &replacement).await?;
            }
            SessionMutationPayload::Delete(tombstone) => {
                let affected = sqlx::query(
                    "DELETE FROM managed_session
                 WHERE session_id = $1 AND scope_id = $2 AND revision = $3",
                )
                .bind(&session_id)
                .bind(owner_scope)
                .bind(db_revision(current_revision)?)
                .execute(&mut *connection)
                .await
                .map_err(storage)?
                .rows_affected();
                if affected != 1 {
                    return Ok((SessionMutationResult::Conflict { current_revision }, false));
                }
                replay::delete_run_archives(&mut *connection, owner_scope, &session_id).await?;
                sqlx::query(
                    "INSERT INTO managed_session_tombstone
                    (session_id, scope_id, deleted_revision, deleted_at, event_admissions_json, private_content)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(tombstone.session_id.as_str())
                .bind(owner_scope)
                .bind(db_revision(tombstone.deleted_revision)?)
                .bind(&tombstone.deleted_at)
                .bind(deleted_admissions)
                .bind(i64::from(private_content))
                .execute(&mut *connection)
                .await
                .map_err(storage)?;
            }
            SessionMutationPayload::EventNoOp { .. } => {}
        }
        sqlx::query(
            "INSERT INTO managed_session_idempotency
            (session_id, idempotency_key, payload_hash, committed_revision, receipt_kind)
         VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&session_id)
        .bind(&mutation.idempotency.key)
        .bind(&mutation.idempotency.payload_hash)
        .bind(db_revision(next)?)
        .bind(receipt_kind_value(receipt_kind))
        .execute(&mut *connection)
        .await
        .map_err(storage)?;
        for fact in mutation.lifecycle_facts {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *connection)
            .await
            .map_err(storage)?;
        }
        Ok((
            if receipt_kind == SessionCommandReceiptKind::EventNoOp {
                SessionMutationResult::AcknowledgedNoOp {
                    observed_revision: next,
                }
            } else {
                SessionMutationResult::Applied { new_revision: next }
            },
            true,
        ))
    }
}

#[cfg(test)]
mod tests;
