// SQLite pending-row serialization and optimistic-revision helpers. These are
// the one helper set used by the parent adapter's Inbox/Outbox implementation.
fn current_revision(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    tx.query_row(
        &format!("SELECT revision FROM {prefix}_pending WHERE message_id = ?1"),
        params![message_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(reject)?
    .map(|revision| durable_u64("pending input revision", revision))
    .transpose()
}
fn pending_for_run(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    run_id: &str,
    now_ms: u64,
) -> Result<Vec<PendingInput>, DispatchError> {
    let mut stmt = tx
        .prepare(&format!(
            "SELECT message_id, thread_id, correlation_id, result, context_messages, available_at \
             FROM {prefix}_pending WHERE run_id = ?1 \
             AND (available_at IS NULL OR available_at <= ?2) ORDER BY created_at"
        ))
        .map_err(reject)?;
    let rows = stmt
        .query_map(params![run_id, crate::clock::db_millis(now_ms)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(reject)?;
    let mut pending = Vec::new();
    for row in rows {
        let (message_id, thread_id, correlation_id, result, context_messages, available_at) =
            row.map_err(reject)?;
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

fn insert_operation(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    operation: &DispatchOperation,
) -> Result<(), DispatchError> {
    let recorded_at_ms = i64::try_from(crate::clock::system_now_ms()).unwrap_or(i64::MAX);
    tx.execute(
        &format!(
            "INSERT INTO {prefix}_dispatch_operation \
             (run_id, operation, recorded_at_ms) VALUES (?1, ?2, ?3)"
        ),
        params![operation.run_id().0, json(operation)?, recorded_at_ms],
    )
    .map_err(reject)?;
    Ok(())
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, DispatchError> {
    serde_json::to_string(value).map_err(json_err)
}

/// The one SQLite pending insert path, reused by direct delivery, Inbox append,
/// and outbox relay so exact retries and identity conflicts cannot diverge.
fn append_pending_row(
    conn: &Connection,
    prefix: &str,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let changed = conn
        .execute(
            &format!(
                "INSERT INTO {prefix}_pending \
                 (message_id, run_id, thread_id, correlation_id, result, context_messages, available_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(message_id) DO NOTHING"
            ),
            params![
                &input.message_id,
                &input.run_id.0,
                &input.thread_id.0,
                &input.correlation_id,
                json(&input.result)?,
                json(&input.context_messages)?,
                input.available_at_ms.map(crate::clock::db_millis)
            ],
        )
        .map_err(reject)?;
    if changed > 0 {
        return Ok(true);
    }
    match load_pending_input(conn, prefix, &input.message_id)? {
        Some(existing) if existing == *input => Ok(false),
        Some(_) => Err(idempotency_conflict(&input.message_id, "pending-input")),
        None => Err(DispatchError::Rejected(format!(
            "pending-input `{}` vanished during idempotency validation",
            input.message_id
        ))),
    }
}

fn load_pending_input(
    conn: &Connection,
    prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError> {
    conn.query_row(
        &format!(
            "SELECT run_id, thread_id, correlation_id, result, context_messages, available_at \
             FROM {prefix}_pending WHERE message_id = ?1"
        ),
        params![message_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        },
    )
    .optional()
    .map_err(reject)?
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
    DispatchError::Rejected(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn json_err(err: serde_json::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
