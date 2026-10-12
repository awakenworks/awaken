// @formal entrypoint ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
// SQLite durable Inbox/Outbox and operational-feed ports. Included by
// `sqlite.rs`; the shared connection/transaction authority stays on the parent store.
use crate::inbox_mutation::{
    PendingInputMutation, PendingInputMutationPlan, plan_pending_input_mutation,
};

type PendingRecordRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
);

#[async_trait]
impl Inbox for SqliteDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let (_authority, mut conn) = self.with_conn().await?;
        append_pending_row(&mut conn, NS, &input).await
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let outcome = match plan_pending_input_mutation(
            current_revision(&mut tx, p, &message_id).await?,
            expected_revision,
            PendingInputMutation::Retract,
            i64::MAX as u64,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => outcome,
            PendingInputMutationPlan::Retract => {
                sqlx::query(&format!("DELETE FROM {p}_pending WHERE message_id = ?1"))
                    .bind(&message_id)
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
        let message_id = message_id.to_string();
        let result_json = json(&result)?;
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let outcome = match plan_pending_input_mutation(
            current_revision(&mut tx, p, &message_id).await?,
            expected_revision,
            PendingInputMutation::Edit,
            i64::MAX as u64,
        )
        .map_err(|error| DispatchError::Rejected(error.to_string()))?
        {
            PendingInputMutationPlan::Unchanged(outcome) => outcome,
            PendingInputMutationPlan::Edit { next_revision } => {
                sqlx::query(&format!(
                    "UPDATE {p}_pending SET result = ?1, revision = ?3 \
                     WHERE message_id = ?2"
                ))
                .bind(&result_json)
                .bind(&message_id)
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
impl awaken_dispatch_contract::InboxRead for SqliteDispatchStore {
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let thread = thread_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let rows = sqlx::query_as::<_, PendingRecordRow>(&format!(
            "SELECT message_id, run_id, correlation_id, result, context_messages, revision, available_at \
             FROM {p}_pending WHERE thread_id = ?1 ORDER BY created_at"
        ))
        .bind(&thread.0)
        .fetch_all(&mut *conn)
        .await
        .map_err(store_error)?;
        let mut records = Vec::new();
        for (
            message_id,
            run_id,
            correlation_id,
            result,
            context_messages,
            revision,
            available_at,
        ) in rows
        {
            records.push(PendingRecord {
                input: PendingInput {
                    message_id,
                    run_id: RunId(run_id),
                    thread_id: thread.clone(),
                    correlation_id,
                    available_at_ms: available_at
                        .map(crate::clock::millis_from_db)
                        .transpose()
                        .map_err(|err| DispatchError::Rejected(err.to_string()))?,
                    result: serde_json::from_str(&result).map_err(json_err)?,
                    context_messages: context_messages
                        .map(|messages| serde_json::from_str::<Vec<Message>>(&messages))
                        .transpose()
                        .map_err(json_err)?
                        .unwrap_or_default(),
                },
                revision: durable_u64("pending input revision", revision)?,
            });
        }
        Ok(records)
    }
}

#[cfg(test)]
mod inbox_revision_tests {
    use super::*;
    use awaken_dispatch_contract::InboxRead;

    #[tokio::test]
    async fn sqlite_pending_edit_never_promotes_exhausted_integer_to_real() {
        // SQL R1 exact MAX BIGINT edit=>typed pre-write rejection, retained
        // INTEGER and bytes; R2 stale edit=>Mismatch; R3 exact retract=>Applied.
        // SQLite's `revision+1` otherwise silently becomes REAL at this edge.
        let store = SqliteDispatchStore::open_in_memory(Arc::new(
            awaken_lease_testkit::ManualClock::new(0),
        ))
        .await
        .unwrap();
        let thread = ThreadId("sqlite-revision-limit".into());
        store
            .append(PendingInput {
                message_id: "sqlite-revision-limit".into(),
                run_id: RunId("run".into()),
                thread_id: thread.clone(),
                correlation_id: "call".into(),
                available_at_ms: None,
                context_messages: Vec::new(),
                result: ResumeResult::Input("old".into()),
            })
            .await
            .unwrap();
        {
            let (_authority, mut conn) = store.with_conn().await.unwrap();
            let p = NS;
            sqlx::query(&format!("UPDATE {p}_pending SET revision = ?1"))
                .bind(i64::MAX)
                .execute(&mut *conn)
                .await
                .map_err(store_error)
                .unwrap();
        }
        let before = store.list(&thread).await.unwrap();
        assert!(
            store
                .edit(
                    "sqlite-revision-limit",
                    i64::MAX as u64,
                    ResumeResult::Input("new".into())
                )
                .await
                .is_err(),
            "R1"
        );
        assert_eq!(
            store.list(&thread).await.unwrap(),
            before,
            "R1 durable bytes/integer intact"
        );
        assert_eq!(
            store
                .edit(
                    "sqlite-revision-limit",
                    1,
                    ResumeResult::Input("new".into())
                )
                .await
                .unwrap(),
            CasOutcome::RevisionMismatch,
            "R2"
        );
        assert_eq!(
            store
                .retract("sqlite-revision-limit", i64::MAX as u64)
                .await
                .unwrap(),
            CasOutcome::Applied,
            "R3"
        );
    }
}
#[async_trait]
impl DispatchOperationalFeed for SqliteDispatchStore {
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
            .await?
            .into_iter()
            .filter_map(crate::dispatch::StoredDispatchEvent::diagnostic)
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(DispatchPage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl Outbox for SqliteDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let payload = json(&input)?;
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let changed = sqlx::query(&format!(
            "INSERT INTO {p}_outbox (message_id, payload) VALUES (?1,?2) \
             ON CONFLICT(message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(payload)
        .execute(&mut *conn)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed > 0 {
            return Ok(true);
        }
        let existing = sqlx::query_scalar::<_, String>(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = ?1"
        ))
        .bind(&input.message_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_error)?
        .map(|stored| serde_json::from_str::<PendingInput>(&stored).map_err(json_err))
        .transpose()?;
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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let staged: Vec<(String, String)> =
            sqlx::query_as(&format!("SELECT message_id, payload FROM {p}_outbox"))
                .fetch_all(&mut *conn)
                .await
                .map_err(store_error)?;

        let mut relayed = 0;
        for (message_id, payload) in staged {
            let input = normalize_pending_millis(
                serde_json::from_str::<PendingInput>(&payload).map_err(json_err)?,
            );
            // One transaction per message: idempotent target append, then
            // drop the outbox row.
            let mut tx = immediate(&mut conn).await?;
            append_pending_row(&mut tx, p, &input).await?;
            sqlx::query(&format!("DELETE FROM {p}_outbox WHERE message_id = ?1"))
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
impl awaken_dispatch_contract::SessionContinuationOutbox for SqliteDispatchStore {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: Option<u64>,
    ) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let session_thread_id = session_thread_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let stored =
            sqlx::query_as::<_, (String, String, i64, Option<String>, Option<i64>)>(&format!(
                "SELECT request, status, cancel_requested, lease_owner, lease_until \
                 FROM {p}_dispatch WHERE run_id = ?1"
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
        let (request, status, cancel_requested, lease_owner, lease_until) = stored;
        let mut request = serde_json::from_str::<RunDispatch>(&request).map_err(json_err)?;
        validate_session_resume_target(
            &request,
            &input,
            &session_thread_id,
            prior_session_activity_epoch,
            session_activity_epoch,
        )?;

        let mut evidence = sqlx::query_scalar::<_, String>(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = ?1 OR \
             (json_extract(payload, '$.run_id') = ?2 AND \
             json_extract(payload, '$.correlation_id') = ?3)"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.correlation_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?
        .into_iter()
        .map(|payload| serde_json::from_str::<PendingInput>(&payload).map_err(json_err))
        .collect::<Result<Vec<_>, _>>()?;
        let pending_ids = sqlx::query_scalar::<_, String>(&format!(
            "SELECT message_id FROM {p}_pending \
             WHERE message_id = ?1 OR \
             (run_id = ?2 AND correlation_id = ?3)"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.correlation_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        for message_id in pending_ids {
            if let Some(pending) = load_pending_input(&mut tx, p, &message_id).await? {
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
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET request = ?1 WHERE run_id = ?2"
        ))
        .bind(json(&request)?)
        .bind(&input.run_id.0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_outbox (message_id, payload) VALUES (?1, ?2)"
        ))
        .bind(&input.message_id)
        .bind(json(&input)?)
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
        let message_id = input.message_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let replay = exact_run_replay(&mut tx, p, &request).await?;
        let staged = sqlx::query_scalar::<_, String>(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = ?1"
        ))
        .bind(&message_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?
        .map(|payload| serde_json::from_str::<PendingInput>(&payload).map_err(json_err))
        .transpose()?;
        let input = normalize_pending_millis(input);
        let existing = staged
            .clone()
            .or(load_pending_input(&mut tx, p, &message_id).await?);
        if existing
            .map(normalize_pending_millis)
            .as_ref()
            .is_some_and(|existing| existing != &input)
        {
            return Err(DispatchError::Conflict(format!(
                "idempotency key `{message_id}` was reused with another continuation payload"
            )));
        }
        validate_outbox_continuation(&input, &request, &admission)?;
        if !replay {
            append_pending_row(&mut tx, p, &input).await?;
            if let ContinuationAdmission::SessionChild(policy) = &admission {
                let parent = session_child_parent(&request)?.clone();
                ensure_session_child_capacity(
                    &request,
                    policy,
                    known_session_child_threads(&mut tx, p, &parent).await?,
                )?;
            }
            insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        }
        if staged.is_some() {
            sqlx::query(&format!("DELETE FROM {p}_outbox WHERE message_id = ?1"))
                .bind(&message_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

// Both purposes use the original operation rows, codec and global sequence.
// Raw batches are decoded before filtering; hidden prefixes cannot consume the
// logical LIMIT or turn unknown/corrupt old rows into silently empty pages.
async fn read_stored_operation_page(
    connection: &mut SqliteConnection,
    cursor: DispatchCursor,
    limit: usize,
    run: Option<&RunId>,
    purpose: crate::dispatch::OperationReadPurpose,
) -> Result<Vec<crate::dispatch::StoredDispatchEvent>, DispatchError> {
    let mut events = Vec::new();
    let mut scanned = cursor;
    while events.len() < limit {
        let after = i64::try_from(scanned.0)
            .map_err(|_| DispatchError::Rejected("dispatch cursor exceeds storage range".into()))?;
        let remaining = i64::try_from(limit - events.len()).unwrap_or(i64::MAX);
        let selection = if run.is_some() { "AND run_id = ?3" } else { "" };
        let sql = format!(
            "SELECT sequence, recorded_at_ms, run_id, operation FROM {NS}_dispatch_operation WHERE sequence > ?1 {selection} ORDER BY sequence LIMIT ?2"
        );
        let mut query = sqlx::query_as::<_, (i64, Option<i64>, String, String)>(&sql)
            .bind(after)
            .bind(remaining);
        if let Some(run) = run {
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
                &encoded,
            )?;
            scanned = event.cursor;
            if event.operation.matches(purpose) {
                events.push(event);
            }
        }
    }
    Ok(events)
}

impl SqliteDispatchStore {
    async fn read_stored_operations(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
        purpose: crate::dispatch::OperationReadPurpose,
    ) -> Result<Vec<crate::dispatch::StoredDispatchEvent>, DispatchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (_authority, mut connection) = self.with_conn().await?;
        read_stored_operation_page(&mut connection, cursor, limit, run, purpose).await
    }
}
