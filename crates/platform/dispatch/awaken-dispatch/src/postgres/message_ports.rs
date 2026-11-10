// @formal entrypoint ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
// PostgreSQL operational-feed and durable Inbox/Outbox ports. Included by
// `postgres.rs` to keep every implementation on PostgresDispatchStore's module path.
use crate::inbox_mutation::{
    PendingInputMutation, PendingInputMutationPlan, plan_pending_input_mutation,
};
#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchAttemptRecordReads for PostgresDispatchStore {
    async fn attempt_admissions_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        session_thread: &ThreadId,
        member_threads: &[ThreadId],
    ) -> Result<DispatchPage, DispatchError> {
        crate::dispatch::validate_admission_scope(session_thread, member_threads)?;
        let events = self
            .read_stored_operations(
                cursor,
                limit,
                None,
                crate::dispatch::OperationReadPurpose::Admissions {
                    session_thread,
                    member_threads,
                },
            )
            .await?;
        Ok(crate::dispatch::diagnostic_page(cursor, events))
    }

    async fn attempt_closure_records_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<crate::AttemptClosurePage, DispatchError> {
        let events = self
            .read_stored_operations(
                cursor,
                limit,
                run,
                crate::dispatch::OperationReadPurpose::AttemptClosure,
            )
            .await?
            .into_iter()
            .filter_map(crate::dispatch::StoredDispatchEvent::attempt_closure)
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(crate::AttemptClosurePage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl DispatchOperationalFeed for PostgresDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        let events = self
            .read_stored_operations(
                cursor,
                limit,
                run,
                crate::dispatch::OperationReadPurpose::Diagnostics,
            )
            .await?;
        Ok(crate::dispatch::diagnostic_page(cursor, events))
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

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let outcome = match plan_pending_input_mutation(
            current_revision(&mut tx, message_id).await?,
            expected_revision,
            PendingInputMutation::Retract,
            i64::MAX as u64,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => outcome,
            PendingInputMutationPlan::Retract => {
                sqlx::query("DELETE FROM runtime_pending WHERE message_id = $1")
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
                CasOutcome::Applied
            }
            PendingInputMutationPlan::Edit { .. } => unreachable!("retract cannot plan edit"),
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
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let outcome = match plan_pending_input_mutation(
            current_revision(&mut tx, message_id).await?,
            expected_revision,
            PendingInputMutation::Edit,
            i64::MAX as u64,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => outcome,
            PendingInputMutationPlan::Edit { next_revision } => {
                sqlx::query(
                    "UPDATE runtime_pending SET result = $1, revision = $3 \
                     WHERE message_id = $2",
                )
                .bind(Json(&result))
                .bind(message_id)
                .bind(durable_i64("pending input revision", next_revision)?)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
                CasOutcome::Applied
            }
            PendingInputMutationPlan::Retract => unreachable!("edit cannot plan retract"),
        };
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}

#[async_trait]
impl awaken_dispatch_contract::InboxRead for PostgresDispatchStore {
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let rows = sqlx::query("SELECT message_id, run_id, correlation_id, result, context_messages, revision, available_at \
             FROM runtime_pending WHERE thread_id = $1 ORDER BY created_at")
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
}

#[async_trait]
impl Outbox for PostgresDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let result = sqlx::query(
            "INSERT INTO runtime_outbox (message_id, payload) VALUES ($1, $2) \
             ON CONFLICT (message_id) DO NOTHING",
        )
        .bind(&input.message_id)
        .bind(Json(&input))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        if result.rows_affected() > 0 {
            return Ok(true);
        }
        let existing = sqlx::query("SELECT payload FROM runtime_outbox WHERE message_id = $1")
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

    async fn relay(&self) -> Result<usize, DispatchError> {
        let staged = sqlx::query("SELECT message_id, payload FROM runtime_outbox")
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
            sqlx::query("DELETE FROM runtime_outbox WHERE message_id = $1")
                .bind(&message_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            tx.commit().await.map_err(store_error)?;
            relayed += 1;
        }
        Ok(relayed)
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionContinuationOutbox for PostgresDispatchStore {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: Option<u64>,
    ) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(
            "SELECT request, status, cancel_requested, lease_owner, lease_until \
             FROM runtime_dispatch WHERE run_id = $1 FOR UPDATE",
        )
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

        let mut evidence = sqlx::query(
            "SELECT payload FROM runtime_outbox \
             WHERE message_id = $1 OR (payload ->> 'run_id' = $2 \
             AND payload ->> 'correlation_id' = $3) FOR UPDATE",
        )
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
        let pending_ids = sqlx::query_scalar::<_, String>(
            "SELECT message_id FROM runtime_pending \
             WHERE message_id = $1 OR \
             (run_id = $2 AND correlation_id = $3) FOR UPDATE",
        )
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

        request.session_activity_epoch = session_activity_epoch;
        sqlx::query("UPDATE runtime_dispatch SET request = $1 WHERE run_id = $2")
            .bind(Json(&request))
            .bind(&input.run_id.0)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        sqlx::query("INSERT INTO runtime_outbox (message_id, payload) VALUES ($1, $2)")
            .bind(&input.message_id)
            .bind(Json(&input))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(true)
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
        let staged =
            sqlx::query("SELECT payload FROM runtime_outbox WHERE message_id = $1 FOR UPDATE")
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
            sqlx::query("DELETE FROM runtime_outbox WHERE message_id = $1")
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
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM runtime_pending WHERE message_id = $1 FOR UPDATE")
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
    operation: impl Into<crate::dispatch::StoredDispatchOperation>,
) -> Result<(), DispatchError> {
    let operation = operation.into();
    // Stamped by the database's authority clock, the one every claim and lease
    // judgement of this store reads, in the same transaction as the transition.
    sqlx::query(
        "INSERT INTO runtime_dispatch_operation (run_id, operation, recorded_at_ms) \
         VALUES ($1, $2, FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint)",
    )
    .bind(&operation.run_id().0)
    .bind(Json(&operation))
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    Ok(())
}

// Every view uses the original operation rows, codec and global sequence.
// Raw batches are decoded before filtering; hidden prefixes cannot consume the
// logical LIMIT or turn unknown/corrupt old rows into silently empty pages.
async fn read_stored_operation_page(
    connection: &mut sqlx::PgConnection,
    cursor: DispatchCursor,
    limit: usize,
    run: Option<&RunId>,
    purpose: crate::dispatch::OperationReadPurpose<'_>,
) -> Result<Vec<crate::dispatch::StoredDispatchEvent>, DispatchError> {
    let mut events = Vec::new();
    let mut scanned = cursor;
    while events.len() < limit {
        let after = i64::try_from(scanned.0)
            .map_err(|_| DispatchError::Rejected("dispatch cursor exceeds storage range".into()))?;
        let remaining = i64::try_from(limit - events.len()).unwrap_or(i64::MAX);
        let selection = match purpose {
            crate::dispatch::OperationReadPurpose::Admissions { .. } => {
                "AND NOT (COALESCE(operation->>'type','') = ANY($5)) \
                 AND (COALESCE(operation->>'type','') <> 'attempt_admitted' \
                   OR operation->'attempt'->>'session_thread_id' = $3 \
                   OR operation->'attempt'->>'thread_id' = ANY($4) \
                   OR jsonb_typeof(operation->'attempt'->'thread_id') IS DISTINCT FROM 'string' \
                   OR NOT (operation->'attempt'->>'thread_id' ~ '[!-~]'))"
            }
            _ if run.is_some() => "AND run_id = $3",
            _ => "",
        };
        let sql = format!(
            "SELECT sequence, recorded_at_ms, run_id, operation FROM {NS}_dispatch_operation WHERE sequence > $1 {selection} ORDER BY sequence LIMIT $2"
        );
        // selection is the closed purpose/run-presence match above and NS is
        // fixed. Cursor, limits, thread/run ids and operation tags remain bound.
        let mut query = sqlx::query_as::<_, (i64, Option<i64>, String, Json<serde_json::Value>)>(
            sqlx::AssertSqlSafe(sql),
        )
        .bind(after)
        .bind(remaining);
        if let crate::dispatch::OperationReadPurpose::Admissions {
            session_thread,
            member_threads,
        } = purpose
        {
            query = query
                .bind(&session_thread.0)
                .bind(
                    member_threads
                        .iter()
                        .map(|id| id.0.clone())
                        .collect::<Vec<_>>(),
                )
                .bind(crate::dispatch::NON_ADMISSION_OPERATION_TAGS);
        } else if let Some(run) = run {
            query = query.bind(&run.0);
        }
        let rows = query
            .fetch_all(&mut *connection)
            .await
            .map_err(store_error)?;
        if rows.is_empty() {
            break;
        }
        for (sequence, recorded_at_ms, run_id, encoded) in rows {
            let event = crate::dispatch::StoredDispatchEvent::decode_row(
                sequence,
                recorded_at_ms,
                &run_id,
                &encoded.0.to_string(),
            )?;
            scanned = event.cursor;
            if event.operation.matches(purpose) {
                events.push(event);
            }
        }
    }
    Ok(events)
}

impl PostgresDispatchStore {
    async fn read_stored_operations(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
        purpose: crate::dispatch::OperationReadPurpose<'_>,
    ) -> Result<Vec<crate::dispatch::StoredDispatchEvent>, DispatchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Preserve the original complete-prefix barrier across every raw
        // batch, not merely the first one: no lower uncommitted allocation can
        // be skipped by a later public or internal checkpoint.
        let mut observation = self.pool.begin().await.map_err(store_error)?;
        sqlx::query("LOCK TABLE runtime_dispatch_operation IN SHARE MODE")
            .execute(&mut *observation)
            .await
            .map_err(store_error)?;
        let events =
            read_stored_operation_page(&mut observation, cursor, limit, run, purpose).await?;
        observation.commit().await.map_err(store_error)?;
        Ok(events)
    }
}

// One physical-retirement + immutable operation append transaction. This
// original Postgres store owns both effects; keeping the transaction beside
// its operation codec/writer adds no second lifecycle or persistence owner.
impl PostgresDispatchStore {
    pub(super) async fn finish_physical_attempt_with(
        &self,
        claim: &RunClaim,
        purpose: crate::dispatch::AttemptFinish<'_>,
    ) -> Result<SettleOutcome, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let epoch = durable_i64("dispatch attempt epoch", claim.epoch)?;
        let current: Option<crate::dispatch::PhysicalAttemptStorageRow<Json<RunDispatch>>> = sqlx::query_as("SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, request, active_attempt_closing FROM runtime_dispatch WHERE run_id = $1 FOR UPDATE")
            .bind(&claim.run_id.0).fetch_optional(&mut *tx).await.map_err(store_error)?;
        let row_exists = current.is_some();
        let (slot, closing, original) =
            if let Some((owner, epoch, binding, request, closing)) = current {
                let epoch = epoch.map(crate::stored_claim_epoch).transpose()?;
                let slot = PhysicalAttemptSlot::from_facts(
                    owner.is_some(),
                    epoch.is_some(),
                    owner.as_deref() == Some(&claim.owner),
                    epoch == Some(claim.epoch),
                );
                let original = if purpose.requires_fact() {
                    let request = request.0;
                    let mut original = crate::dispatch::decode_physical_attempt(
                        &claim.run_id,
                        owner,
                        epoch,
                        binding,
                        closing,
                    )?;
                    if let Some(original) = &mut original {
                        original.thread_id = Some(request.thread_id().clone());
                        original.session_thread_id = request.session_thread_id;
                    }
                    original
                } else {
                    None
                };
                (slot, closing, original)
            } else {
                (PhysicalAttemptSlot::Empty, false, None)
            };
        let prior = if purpose.requires_fact() {
            read_stored_operation_page(
                &mut tx,
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id),
                crate::dispatch::OperationReadPurpose::AttemptClosure,
            )
            .await?
        } else {
            Vec::new()
        };
        let (transition, outcome, fact) = crate::dispatch::plan_physical_attempt_finish(
            claim,
            purpose,
            slot,
            closing,
            original.as_ref(),
            prior
                .iter()
                .filter_map(|event| event.operation.physical_fact()),
            row_exists,
        )?;
        if transition == PhysicalAttemptTransition::Clear {
            // Both append and exact slot retirement share this original
            // transaction. Append failure cannot discard the original binding.
            if let Some(fact) = &fact {
                insert_operation(&mut tx, fact).await?;
            }
            sqlx::query("UPDATE runtime_dispatch SET active_attempt_owner = NULL, active_attempt_epoch = NULL, active_attempt_binding = NULL, active_attempt_closing = FALSE, retired_attempt_epoch = $3 WHERE run_id = $1 AND active_attempt_owner = $2 AND active_attempt_epoch = $3")
                .bind(&claim.run_id.0).bind(&claim.owner).bind(epoch).execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}
