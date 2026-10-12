// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: SQLite pending-row transition helpers. Dynamic path: durable pending work
// @formal reason: moves through eligibility, lease, consumption, and retry under the canonical ingress
// @formal reason: transaction. Evidence: dispatch.durable_ingress.claim_and_delivery and SQLite recovery
// @formal reason: tests; this is a product-requirement classification, not a direct proof claim.
// SQLite pending-row serialization and optimistic-revision helpers. These are
// the one helper set used by the parent adapter's Inbox/Outbox implementation.
async fn current_revision(
    tx: &mut SqliteConnection,
    prefix: &str,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT revision FROM {prefix}_pending WHERE message_id = ?1"
    ))
    .bind(message_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_error)?
    .map(|revision| durable_u64("pending input revision", revision))
    .transpose()
}

type PendingForRunRow = (String, String, String, String, Option<String>, Option<i64>);

async fn pending_for_run(
    tx: &mut SqliteConnection,
    prefix: &str,
    run_id: &str,
    now_ms: u64,
) -> Result<Vec<PendingInput>, DispatchError> {
    let rows = sqlx::query_as::<_, PendingForRunRow>(&format!(
        "SELECT message_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM {prefix}_pending WHERE run_id = ?1 \
         AND (available_at IS NULL OR available_at <= ?2) ORDER BY created_at"
    ))
    .bind(run_id)
    .bind(crate::clock::db_millis(now_ms))
    .fetch_all(&mut *tx)
    .await
    .map_err(store_error)?;
    let mut pending = Vec::new();
    for (message_id, thread_id, correlation_id, result, context_messages, available_at) in rows {
        pending.push(PendingInput {
            message_id,
            run_id: RunId(run_id.to_string()),
            thread_id: ThreadId(thread_id),
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
        });
    }
    Ok(pending)
}

/// Record `operation` at `recorded_at_ms`: the store clock's reading for the
/// transition it records, the same reading that transition was judged on.
async fn insert_operation(
    tx: &mut SqliteConnection,
    prefix: &str,
    operation: impl Into<crate::dispatch::StoredDispatchOperation>,
    recorded_at_ms: u64,
) -> Result<(), DispatchError> {
    let operation = operation.into();
    let recorded_at_ms = crate::clock::db_millis(recorded_at_ms);
    sqlx::query(&format!(
        "INSERT INTO {prefix}_dispatch_operation \
         (run_id, operation, recorded_at_ms) VALUES (?1, ?2, ?3)"
    ))
    .bind(&operation.run_id().0)
    .bind(json(&operation)?)
    .bind(recorded_at_ms)
    .execute(&mut *tx)
    .await
    .map_err(store_error)?;
    Ok(())
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, DispatchError> {
    serde_json::to_string(value).map_err(json_err)
}

/// The one SQLite pending insert path, reused by direct delivery, Inbox append,
/// and outbox relay so exact retries and identity conflicts cannot diverge.
async fn append_pending_row(
    conn: &mut SqliteConnection,
    prefix: &str,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let changed = sqlx::query(&format!(
        "INSERT INTO {prefix}_pending \
         (message_id, run_id, thread_id, correlation_id, result, context_messages, available_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(message_id) DO NOTHING"
    ))
    .bind(&input.message_id)
    .bind(&input.run_id.0)
    .bind(&input.thread_id.0)
    .bind(&input.correlation_id)
    .bind(json(&input.result)?)
    .bind(json(&input.context_messages)?)
    .bind(input.available_at_ms.map(crate::clock::db_millis))
    .execute(&mut *conn)
    .await
    .map_err(store_error)?
    .rows_affected();
    if changed > 0 {
        return Ok(true);
    }
    match load_pending_input(conn, prefix, &input.message_id).await? {
        Some(existing) if existing == *input => Ok(false),
        Some(_) => Err(idempotency_conflict(&input.message_id, "pending-input")),
        None => Err(DispatchError::Rejected(format!(
            "pending-input `{}` vanished during idempotency validation",
            input.message_id
        ))),
    }
}

async fn load_pending_input(
    conn: &mut SqliteConnection,
    prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError> {
    sqlx::query_as::<_, (String, String, String, String, Option<String>, Option<i64>)>(&format!(
        "SELECT run_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM {prefix}_pending WHERE message_id = ?1"
    ))
    .bind(message_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(store_error)?
    .map(
        |(run_id, thread_id, correlation_id, result, context_messages, available_at)| {
            Ok(PendingInput {
                message_id: message_id.to_string(),
                run_id: RunId(run_id),
                thread_id: ThreadId(thread_id),
                correlation_id,
                available_at_ms: available_at.map(u64::try_from).transpose().map_err(|_| {
                    DispatchError::Rejected(format!(
                        "pending-input `{message_id}` has a negative delivery time"
                    ))
                })?,
                result: serde_json::from_str(&result).map_err(json_err)?,
                context_messages: context_messages
                    .map(|messages| serde_json::from_str::<Vec<Message>>(&messages))
                    .transpose()
                    .map_err(json_err)?
                    .unwrap_or_default(),
            })
        },
    )
    .transpose()
}

fn idempotency_conflict(message_id: &str, aggregate: &str) -> DispatchError {
    DispatchError::Conflict(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn json_err(err: serde_json::Error) -> DispatchError {
    DispatchError::unavailable(err)
}
