// SQLite durable Inbox/Outbox and operational-feed ports. Included by
// `sqlite.rs`; the shared connection/transaction authority stays on the parent store.
use crate::inbox_mutation::{
    PendingInputMutation, PendingInputMutationPlan, plan_pending_input_mutation,
};
#[async_trait]
impl Inbox for SqliteDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        self.with_conn(move |conn, p| append_pending_row(conn, p, &input))
            .await
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let thread = thread_id.clone();
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT message_id, run_id, correlation_id, result, context_messages, revision, available_at \
                     FROM {p}_pending WHERE thread_id = ?1 ORDER BY created_at"
                ))
                .map_err(store_error)?;
            let rows = stmt
                .query_map(params![thread.0], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                    ))
                })
                .map_err(store_error)?;
            let mut records = Vec::new();
            for row in rows {
                let (
                    message_id,
                    run_id,
                    correlation_id,
                    result,
                    context_messages,
                    revision,
                    available_at,
                ) =
                    row.map_err(store_error)?;
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
        })
        .await
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let outcome = match plan_pending_input_mutation(
                current_revision(&tx, p, &message_id)?,
                expected_revision,
                PendingInputMutation::Retract,
                i64::MAX as u64,
            )
            .map_err(|error| DispatchError::Rejected(error.to_string()))?
            {
                PendingInputMutationPlan::Unchanged(outcome) => outcome,
                PendingInputMutationPlan::Retract => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                        params![message_id],
                    )
                    .map_err(store_error)?;
                    CasOutcome::Applied
                }
                PendingInputMutationPlan::Edit { .. } => unreachable!("retract cannot plan edit"),
            };
            tx.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        let result_json = json(&result)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let outcome = match plan_pending_input_mutation(
                current_revision(&tx, p, &message_id)?,
                expected_revision,
                PendingInputMutation::Edit,
                i64::MAX as u64,
            )
            .map_err(|error| DispatchError::Rejected(error.to_string()))?
            {
                PendingInputMutationPlan::Unchanged(outcome) => outcome,
                PendingInputMutationPlan::Edit { next_revision } => {
                    tx.execute(
                        &format!(
                            "UPDATE {p}_pending SET result = ?1, revision = ?3 \
                             WHERE message_id = ?2"
                        ),
                        params![
                            result_json,
                            message_id,
                            durable_i64("pending input revision", next_revision)?
                        ],
                    )
                    .map_err(store_error)?;
                    CasOutcome::Applied
                }
                PendingInputMutationPlan::Retract => unreachable!("edit cannot plan retract"),
            };
            tx.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }
}

#[cfg(test)]
mod inbox_revision_tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_pending_edit_never_promotes_exhausted_integer_to_real() {
        // SQL R1 exact MAX BIGINT edit=>typed pre-write rejection, retained
        // INTEGER and bytes; R2 stale edit=>Mismatch; R3 exact retract=>Applied.
        // SQLite's `revision+1` otherwise silently becomes REAL at this edge.
        let store =
            SqliteDispatchStore::open_in_memory(Arc::new(crate::clock::ManualClock::new(0)))
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
        store
            .with_conn(|conn, p| {
                conn.execute(
                    &format!("UPDATE {p}_pending SET revision = ?1"),
                    params![i64::MAX],
                )
                .map_err(store_error)?;
                Ok(())
            })
            .await
            .unwrap();
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
    ) -> Result<DispatchPage, DispatchError> {
        if limit == 0 {
            return Ok(DispatchPage {
                events: Vec::new(),
                next_cursor: cursor,
            });
        }
        let after = i64::try_from(cursor.0).map_err(|_| {
            DispatchError::Rejected("dispatch cursor exceeds INTEGER range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn, prefix| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT sequence, recorded_at_ms, operation FROM {prefix}_dispatch_operation \
                     WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
                ))
                .map_err(store_error)?;
            let rows = statement
                .query_map(params![after, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(store_error)?;
            let mut events = Vec::new();
            for row in rows {
                let (sequence, recorded_at_ms, operation) = row.map_err(store_error)?;
                events.push(DispatchOperationalEvent {
                    cursor: DispatchCursor(u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation sequence is negative".to_string(),
                        )
                    })?),
                    recorded_at_ms: recorded_at_ms.map(u64::try_from).transpose().map_err(
                        |_| {
                            DispatchError::Rejected(
                                "persisted dispatch operation time is negative".to_string(),
                            )
                        },
                    )?,
                    operation: serde_json::from_str(&operation).map_err(json_err)?,
                });
            }
            let next_cursor = events.last().map_or(cursor, |event| event.cursor);
            Ok(DispatchPage {
                events,
                next_cursor,
            })
        })
        .await
    }
}

#[async_trait]
impl Outbox for SqliteDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let payload = json(&input)?;
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_outbox (message_id, payload) VALUES (?1,?2) \
                         ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![&input.message_id, payload],
                )
                .map_err(store_error)?;
            if changed > 0 {
                return Ok(true);
            }
            let existing = conn
                .query_row(
                    &format!("SELECT payload FROM {p}_outbox WHERE message_id = ?1"),
                    params![&input.message_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
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
        })
        .await
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let staged: Vec<(String, String)> = {
                let mut stmt = conn
                    .prepare(&format!("SELECT message_id, payload FROM {p}_outbox"))
                    .map_err(store_error)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(store_error)?;
                rows.collect::<Result<_, _>>().map_err(store_error)?
            };

            let mut relayed = 0;
            for (message_id, payload) in staged {
                let input = normalize_pending_millis(
                    serde_json::from_str::<PendingInput>(&payload).map_err(json_err)?,
                );
                // One transaction per message: idempotent target append, then
                // drop the outbox row.
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(store_error)?;
                append_pending_row(&tx, p, &input)?;
                tx.execute(
                    &format!("DELETE FROM {p}_outbox WHERE message_id = ?1"),
                    params![message_id],
                )
                .map_err(store_error)?;
                tx.commit().map_err(store_error)?;
                relayed += 1;
            }
            Ok(relayed)
        })
        .await
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
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let stored = tx
                .query_row(
                    &format!(
                        "SELECT request, status, cancel_requested, lease_owner, lease_until \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![&input.run_id.0],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .optional()
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

            let mut evidence = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT payload FROM {p}_outbox WHERE message_id = ?1 OR \
                         (json_extract(payload, '$.run_id') = ?2 AND \
                         json_extract(payload, '$.correlation_id') = ?3)"
                    ))
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(
                        params![&input.message_id, &input.run_id.0, &input.correlation_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(store_error)?;
                rows.map(|row| {
                    row.map_err(store_error).and_then(|payload| {
                        serde_json::from_str::<PendingInput>(&payload).map_err(json_err)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
            };
            let pending_ids = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT message_id FROM {p}_pending \
                         WHERE message_id = ?1 OR \
                         (run_id = ?2 AND correlation_id = ?3)"
                    ))
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(
                        params![&input.message_id, &input.run_id.0, &input.correlation_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(store_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(store_error)?
            };
            for message_id in pending_ids {
                if let Some(pending) = load_pending_input(&tx, p, &message_id)? {
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
            tx.execute(
                &format!("UPDATE {p}_dispatch SET request = ?1 WHERE run_id = ?2"),
                params![json(&request)?, &input.run_id.0],
            )
            .map_err(store_error)?;
            tx.execute(
                &format!("INSERT INTO {p}_outbox (message_id, payload) VALUES (?1, ?2)"),
                params![&input.message_id, json(&input)?],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(true)
        })
        .await
    }

    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        let message_id = input.message_id.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let replay = exact_run_replay(&tx, p, &request)?;
            let staged = tx
                .query_row(
                    &format!("SELECT payload FROM {p}_outbox WHERE message_id = ?1"),
                    params![message_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?
                .map(|payload| serde_json::from_str::<PendingInput>(&payload).map_err(json_err))
                .transpose()?;
            let input = normalize_pending_millis(input);
            let existing = staged.clone().or(load_pending_input(&tx, p, &message_id)?);
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
                append_pending_row(&tx, p, &input)?;
                if let ContinuationAdmission::SessionChild(policy) = &admission {
                    let parent = session_child_parent(&request)?.clone();
                    ensure_session_child_capacity(
                        &request,
                        policy,
                        known_session_child_threads(&tx, p, &parent)?,
                    )?;
                }
                insert_new_dispatch(&tx, p, &request, &SubmitOptions::default())?;
            }
            if staged.is_some() {
                tx.execute(
                    &format!("DELETE FROM {p}_outbox WHERE message_id = ?1"),
                    params![message_id],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(())
        })
        .await
    }
}
