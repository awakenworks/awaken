// PostgreSQL operational-feed and durable Inbox/Outbox ports. Included by
// `postgres.rs` to keep every implementation on PostgresDispatchStore's module path.
#[async_trait]
impl DispatchOperationalFeed for PostgresDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError> {
        if limit == 0 {
            return Ok(DispatchPage {
                events: Vec::new(),
                next_cursor: cursor,
            });
        }
        let after = i64::try_from(cursor.0).map_err(|_| {
            DispatchError::Rejected("dispatch cursor exceeds BIGINT range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let prefix = NS;
        let rows = sqlx::query(&format!(
            "SELECT sequence, recorded_at_ms, operation FROM {prefix}_dispatch_operation \
             WHERE sequence > $1 ORDER BY sequence LIMIT $2"
        ))
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        let events = rows
            .into_iter()
            .map(|row| {
                let sequence = row.try_get::<i64, _>("sequence").map_err(store_error)?;
                let recorded_at_ms = row
                    .try_get::<Option<i64>, _>("recorded_at_ms")
                    .map_err(store_error)?
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation time is negative".to_string(),
                        )
                    })?;
                let Json(operation): Json<DispatchOperation> =
                    row.try_get("operation").map_err(store_error)?;
                Ok(DispatchOperationalEvent {
                    cursor: DispatchCursor(u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation sequence is negative".to_string(),
                        )
                    })?),
                    recorded_at_ms,
                    operation,
                })
            })
            .collect::<Result<Vec<_>, DispatchError>>()?;
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(DispatchPage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl Inbox for PostgresDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let inserted = append_pending_transaction(&mut tx, NS, &input).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(inserted)
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT message_id, run_id, correlation_id, result, context_messages, revision, available_at \
             FROM {p}_pending WHERE thread_id = $1 ORDER BY created_at"
        ))
        .bind(&thread_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let message_id: String = row.try_get("message_id").map_err(store_error)?;
            let run_id: String = row.try_get("run_id").map_err(store_error)?;
            let correlation_id: String = row.try_get("correlation_id").map_err(store_error)?;
            let Json(result): Json<ResumeResult> = row.try_get("result").map_err(store_error)?;
            let context_messages = row
                .try_get::<Option<Json<Vec<Message>>>, _>("context_messages")
                .map_err(store_error)?
                .map(|Json(messages)| messages)
                .unwrap_or_default();
            let revision: i64 = row.try_get("revision").map_err(store_error)?;
            let available_at: Option<i64> = row.try_get("available_at").map_err(store_error)?;
            records.push(PendingRecord {
                input: PendingInput {
                    message_id,
                    run_id: RunId(run_id),
                    thread_id: thread_id.clone(),
                    correlation_id,
                    available_at_ms: available_at
                        .map(crate::clock::millis_from_db)
                        .transpose()
                        .map_err(|err| DispatchError::Rejected(err.to_string()))?,
                    result,
                    context_messages,
                },
                revision: durable_u64("pending input revision", revision)?,
            });
        }
        Ok(records)
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let outcome = match current_revision(&mut tx, message_id).await? {
            None => CasOutcome::NotFound,
            Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
            Some(_) => {
                sqlx::query(&format!("DELETE FROM {p}_pending WHERE message_id = $1"))
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
                CasOutcome::Applied
            }
        };
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let outcome = match current_revision(&mut tx, message_id).await? {
            None => CasOutcome::NotFound,
            Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
            Some(_) => {
                sqlx::query(&format!(
                    "UPDATE {p}_pending SET result = $1, revision = revision + 1 \
                     WHERE message_id = $2"
                ))
                .bind(Json(&result))
                .bind(message_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
                CasOutcome::Applied
            }
        };
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}

#[async_trait]
impl Outbox for PostgresDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let p = NS;
        let result = sqlx::query(&format!(
            "INSERT INTO {p}_outbox (message_id, payload) VALUES ($1, $2) \
             ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(Json(&input))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        if result.rows_affected() > 0 {
            return Ok(true);
        }
        let existing = sqlx::query(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = $1"
        ))
        .bind(&input.message_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?
        .map(|row| {
            row.try_get::<Json<PendingInput>, _>("payload")
                .map(|value| value.0)
        })
        .transpose()
        .map_err(store_error)?;
        match existing {
            Some(existing) if existing == input => Ok(false),
            Some(_) => Err(idempotency_conflict(&input.message_id, "outbox")),
            None => Err(DispatchError::Rejected(format!(
                "outbox `{}` vanished during idempotency validation",
                input.message_id
            ))),
        }
    }

    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: u64,
    ) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(&format!(
            "SELECT request, status, cancel_requested, lease_owner, lease_until \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&input.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?
        .ok_or_else(|| {
            DispatchError::Rejected(format!(
                "Session resume Run `{}` was not found",
                input.run_id.0
            ))
        })?;
        let Json(mut request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
        validate_session_resume_target(
            &request,
            &input,
            session_thread_id,
            prior_session_activity_epoch,
            session_activity_epoch,
        )?;
        let status: String = row.try_get("status").map_err(store_error)?;
        let cancel_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
        let lease_owner: Option<String> = row.try_get("lease_owner").map_err(store_error)?;
        let lease_until: Option<i64> = row.try_get("lease_until").map_err(store_error)?;

        let mut evidence = sqlx::query(&format!(
            "SELECT payload FROM {p}_outbox \
             WHERE message_id = $1 OR (payload ->> 'run_id' = $2 \
             AND payload ->> 'correlation_id' = $3) FOR UPDATE"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.correlation_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?
        .into_iter()
        .map(|row| {
            row.try_get::<Json<PendingInput>, _>("payload")
                .map(|Json(input)| input)
                .map_err(store_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
        let pending_ids = sqlx::query_scalar::<_, String>(&format!(
            "SELECT message_id FROM {p}_pending \
             WHERE message_id = $1 OR \
             (run_id = $2 AND correlation_id = $3) FOR UPDATE"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.correlation_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        for message_id in pending_ids {
            if let Some(pending) = load_pending_input(&mut *tx, p, &message_id).await? {
                evidence.push(pending);
            }
        }
        let exact = validate_session_resume_evidence(&input, evidence.iter())?;
        if !validate_session_resume_activity_transition(
            request.session_activity_epoch,
            prior_session_activity_epoch,
            session_activity_epoch,
            exact,
        )? {
            return Ok(false);
        }
        let accepts_new_resume = cancel_requested == 0
            && (((status == "pending" || status == "awaiting")
                && lease_owner.is_none()
                && lease_until.is_none())
                || (status == "running" && lease_owner.is_some() && lease_until.is_some()));
        if !accepts_new_resume {
            return Err(DispatchError::Rejected(
                "Session resume requires a Pending, Awaiting, or currently Leased dispatch"
                    .to_string(),
            ));
        }

        request.session_activity_epoch = Some(session_activity_epoch);
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET request = $1 WHERE run_id = $2"
        ))
        .bind(Json(&request))
        .bind(&input.run_id.0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_outbox (message_id, payload) VALUES ($1, $2)"
        ))
        .bind(&input.message_id)
        .bind(Json(&input))
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(true)
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        let p = NS;
        let staged = sqlx::query(&format!("SELECT message_id, payload FROM {p}_outbox"))
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;

        let mut relayed = 0;
        for row in staged {
            let message_id: String = row.try_get("message_id").map_err(store_error)?;
            let Json(input): Json<PendingInput> = row.try_get("payload").map_err(store_error)?;
            let input = normalize_pending_millis(input);

            // One transaction per message: idempotent target append, then drop
            // the outbox row. A crash before the delete re-appends (a no-op).
            let mut tx = self.pool.begin().await.map_err(store_error)?;
            append_pending_transaction(&mut tx, NS, &input).await?;
            sqlx::query(&format!("DELETE FROM {p}_outbox WHERE message_id = $1"))
                .bind(&message_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            tx.commit().await.map_err(store_error)?;
            relayed += 1;
        }
        Ok(relayed)
    }

    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let message_id = input.message_id.clone();
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_run_identity(&mut tx, &request.run_id().0).await?;
        let replay = exact_run_replay(&mut tx, p, &request).await?;
        let staged = sqlx::query(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = $1 FOR UPDATE"
        ))
        .bind(&message_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?
        .map(|row| row.try_get::<Json<PendingInput>, _>("payload"))
        .transpose()
        .map_err(store_error)?
        .map(|Json(input)| normalize_pending_millis(input));
        let existing = match staged.clone() {
            Some(input) => Some(input),
            None => load_pending_input(&mut *tx, p, &message_id).await?,
        };
        let input = normalize_pending_millis(input);
        if existing.as_ref().is_some_and(|existing| existing != &input) {
            return Err(DispatchError::Conflict(format!(
                "idempotency key `{message_id}` was reused with another continuation payload"
            )));
        }
        validate_outbox_continuation(&input, &request, &admission)?;
        if !replay {
            append_pending_transaction(&mut tx, p, &input).await?;
            if let ContinuationAdmission::SessionChild(policy) = &admission {
                admit_session_child(&mut tx, p, &request, policy).await?;
            }
            insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        }
        if staged.is_some() {
            sqlx::query(&format!("DELETE FROM {p}_outbox WHERE message_id = $1"))
                .bind(&message_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

async fn current_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    let revision: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT revision FROM {NS}_pending WHERE message_id = $1"
    ))
    .bind(message_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?;
    revision
        .map(|revision| durable_u64("pending input revision", revision))
        .transpose()
}

async fn insert_operation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    operation: &DispatchOperation,
) -> Result<(), DispatchError> {
    let prefix = NS;
    let recorded_at_ms = i64::try_from(crate::clock::system_now_ms()).unwrap_or(i64::MAX);
    sqlx::query(&format!(
        "INSERT INTO {prefix}_dispatch_operation (run_id, operation, recorded_at_ms) \
         VALUES ($1, $2, $3)"
    ))
    .bind(&operation.run_id().0)
    .bind(Json(operation))
    .bind(recorded_at_ms)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    Ok(())
}
