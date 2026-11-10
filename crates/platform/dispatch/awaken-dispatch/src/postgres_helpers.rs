// @formal surface obligation=ingress.pending_input_exactly_once
// @formal surface obligation=run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal reason: PostgreSQL transactional pending-input idempotency and retry-exhaustion candidate
// @formal reason: selection.
//! Focused PostgreSQL transaction helpers shared by dispatch command paths.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::types::Json;
use sqlx::{Executor, PgPool, Postgres, Row};

use crate::{DispatchError, PendingInput, RunClaim, WorkerIdentity};

pub(super) async fn postgres_now_ms<'e, E>(executor: E) -> Result<u64, DispatchError>
where
    E: Executor<'e, Database = Postgres>,
{
    let now: i64 =
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(executor)
            .await
            .map_err(store_error)?;
    crate::clock::millis_from_db(now).map_err(|error| DispatchError::Rejected(error.to_string()))
}

pub(super) async fn current_worker_claim(
    pool: &PgPool,
    _prefix: &str,
    identity: &WorkerIdentity,
    run_id: &RunId,
    _now_ms: u64,
) -> Result<Option<RunClaim>, DispatchError> {
    let owner = identity.lease_owner();
    let epoch = sqlx::query_scalar::<_, i64>(
        "SELECT lease_epoch FROM runtime_dispatch WHERE run_id = $1 AND status = 'running' \
         AND lease_owner = $2 AND lease_until IS NOT NULL \
         AND lease_until > FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint \
         AND cancel_requested = 0",
    )
    .bind(&run_id.0)
    .bind(&owner)
    .fetch_optional(pool)
    .await
    .map_err(store_error)?;
    epoch
        .map(|epoch| {
            Ok(RunClaim {
                run_id: run_id.clone(),
                owner,
                epoch: crate::stored_claim_epoch(epoch)?,
            })
        })
        .transpose()
}

pub(super) async fn retry_exhausted_candidate(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    _prefix: &str,
    now_ms: u64,
    max_attempts: i64,
) -> Result<Option<String>, DispatchError> {
    sqlx::query_scalar::<_, String>(
        "SELECT run_id FROM runtime_dispatch \
         WHERE status = 'running' AND lease_until IS NOT NULL \
         AND lease_until <= $1 AND attempt_count >= $2 \
         AND active_attempt_epoch IS NULL \
         ORDER BY created_at LIMIT 1",
    )
    .bind(crate::clock::db_millis(now_ms))
    .bind(max_attempts)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)
}

/// The one PostgreSQL pending insert path, reused by direct delivery, Inbox
/// append, and outbox relay so retry/conflict semantics stay transactional.
pub(super) async fn append_pending_transaction(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    prefix: &str,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let inserted = sqlx::query(
        "INSERT INTO runtime_pending \
         (message_id, run_id, thread_id, correlation_id, result, context_messages, available_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(&input.message_id)
    .bind(&input.run_id.0)
    .bind(&input.thread_id.0)
    .bind(&input.correlation_id)
    .bind(Json(&input.result))
    .bind(Json(&input.context_messages))
    .bind(input.available_at_ms.map(crate::clock::db_millis))
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
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
    _prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(
        "SELECT run_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM runtime_pending WHERE message_id = $1",
    )
    .bind(message_id)
    .fetch_optional(executor)
    .await
    .map_err(store_error)?;
    row.map(|row| {
        let available_at = row
            .try_get::<Option<i64>, _>("available_at")
            .map_err(store_error)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                DispatchError::Rejected(format!(
                    "pending-input `{message_id}` has a negative delivery time"
                ))
            })?;
        let Json(result): Json<ResumeResult> = row.try_get("result").map_err(store_error)?;
        let context_messages = row
            .try_get::<Option<Json<Vec<Message>>>, _>("context_messages")
            .map_err(store_error)?
            .map(|Json(messages)| messages)
            .unwrap_or_default();
        Ok(PendingInput {
            message_id: message_id.to_string(),
            run_id: RunId(row.try_get("run_id").map_err(store_error)?),
            thread_id: ThreadId(row.try_get("thread_id").map_err(store_error)?),
            correlation_id: row.try_get("correlation_id").map_err(store_error)?,
            available_at_ms: available_at,
            result,
            context_messages,
        })
    })
    .transpose()
}

pub(super) fn idempotency_conflict(message_id: &str, aggregate: &str) -> DispatchError {
    DispatchError::Conflict(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn store_error(error: sqlx::Error) -> DispatchError {
    DispatchError::unavailable(error)
}
