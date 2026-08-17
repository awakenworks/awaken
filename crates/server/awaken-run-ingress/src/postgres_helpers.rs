//! Focused PostgreSQL transaction helpers shared by dispatch command paths.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::types::Json;
use sqlx::{Executor, Postgres, Row};

use crate::{DispatchError, PendingInput};

pub(super) async fn retry_exhausted_candidate(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    prefix: &str,
    now_ms: u64,
    max_attempts: i64,
) -> Result<Option<String>, DispatchError> {
    sqlx::query_scalar::<_, String>(&format!(
        "SELECT run_id FROM {prefix}_dispatch \
         WHERE status = 'running' AND lease_until IS NOT NULL \
         AND lease_until < $1 AND attempt_count >= $2 \
         ORDER BY created_at LIMIT 1"
    ))
    .bind(crate::clock::db_millis(now_ms))
    .bind(max_attempts)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)
}

/// The one PostgreSQL pending insert path, reused by direct delivery, Inbox
/// append, and outbox relay so retry/conflict semantics stay transactional.
pub(super) async fn append_pending_transaction(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    prefix: &str,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let inserted = sqlx::query(&format!(
        "INSERT INTO {prefix}_pending \
         (message_id, run_id, thread_id, correlation_id, result, available_at) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (message_id) DO NOTHING"
    ))
    .bind(&input.message_id)
    .bind(&input.run_id.0)
    .bind(&input.thread_id.0)
    .bind(&input.correlation_id)
    .bind(Json(&input.result))
    .bind(input.available_at_ms.map(crate::clock::db_millis))
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    if inserted.rows_affected() > 0 {
        return Ok(true);
    }
    match load_pending_input(&mut **tx, prefix, &input.message_id).await? {
        Some(existing) if existing == *input => Ok(false),
        Some(_) => Err(idempotency_conflict(&input.message_id, "pending-input")),
        None => Err(DispatchError::Rejected(format!(
            "pending-input `{}` vanished during idempotency validation",
            input.message_id
        ))),
    }
}

async fn load_pending_input<'e, E>(
    executor: E,
    prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(&format!(
        "SELECT run_id, thread_id, correlation_id, result, available_at \
         FROM {prefix}_pending WHERE message_id = $1"
    ))
    .bind(message_id)
    .fetch_optional(executor)
    .await
    .map_err(reject)?;
    row.map(|row| {
        let available_at = row
            .try_get::<Option<i64>, _>("available_at")
            .map_err(reject)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                DispatchError::Rejected(format!(
                    "pending-input `{message_id}` has a negative delivery time"
                ))
            })?;
        let Json(result): Json<ResumeResult> = row.try_get("result").map_err(reject)?;
        Ok(PendingInput {
            message_id: message_id.to_string(),
            run_id: RunId(row.try_get("run_id").map_err(reject)?),
            thread_id: ThreadId(row.try_get("thread_id").map_err(reject)?),
            correlation_id: row.try_get("correlation_id").map_err(reject)?,
            available_at_ms: available_at,
            result,
        })
    })
    .transpose()
}

pub(super) fn idempotency_conflict(message_id: &str, aggregate: &str) -> DispatchError {
    DispatchError::Rejected(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn reject(error: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(error.to_string())
}
