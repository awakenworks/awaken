//! Focused PostgreSQL transaction helpers shared by dispatch command paths.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::types::Json;
use sqlx::{Executor, PgPool, Postgres, Row};

use crate::{DispatchError, PendingInput, RunClaim, WorkerIdentity, durable_u64};

pub(super) async fn current_worker_claim(
    pool: &PgPool,
    prefix: &str,
    identity: &WorkerIdentity,
    run_id: &RunId,
    now_ms: u64,
) -> Result<Option<RunClaim>, DispatchError> {
    let owner = identity.lease_owner();
    let epoch = sqlx::query_scalar::<_, i64>(&format!(
        "SELECT lease_epoch FROM {prefix}_dispatch WHERE run_id = $1 AND status = 'running' \
         AND lease_owner = $2 AND lease_until IS NOT NULL \
         AND lease_until >= $3 AND cancel_requested = 0"
    ))
    .bind(&run_id.0)
    .bind(&owner)
    .bind(crate::clock::db_millis(now_ms))
    .fetch_optional(pool)
    .await
    .map_err(reject)?;
    epoch
        .map(|epoch| {
            Ok(RunClaim {
                run_id: run_id.clone(),
                owner,
                epoch: durable_u64("dispatch lease epoch", epoch)?,
            })
        })
        .transpose()
}

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
         (message_id, run_id, thread_id, correlation_id, result, context_messages, available_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (message_id) DO NOTHING"
    ))
    .bind(&input.message_id)
    .bind(&input.run_id.0)
    .bind(&input.thread_id.0)
    .bind(&input.correlation_id)
    .bind(Json(&input.result))
    .bind(Json(&input.context_messages))
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

pub(super) async fn load_pending_input<'e, E>(
    executor: E,
    prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(&format!(
        "SELECT run_id, thread_id, correlation_id, result, context_messages, available_at \
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
        let context_messages = row
            .try_get::<Option<Json<Vec<Message>>>, _>("context_messages")
            .map_err(reject)?
            .map(|Json(messages)| messages)
            .unwrap_or_default();
        Ok(PendingInput {
            message_id: message_id.to_string(),
            run_id: RunId(row.try_get("run_id").map_err(reject)?),
            thread_id: ThreadId(row.try_get("thread_id").map_err(reject)?),
            correlation_id: row.try_get("correlation_id").map_err(reject)?,
            available_at_ms: available_at,
            result,
            context_messages,
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
