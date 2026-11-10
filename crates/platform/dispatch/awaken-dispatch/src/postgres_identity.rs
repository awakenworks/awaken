// @formal surface obligation=outcome.stable_run_identity_and_exactly_once_ack
// @formal reason: PostgreSQL evidence adapter for serialized, replay-safe Run identity admission and
// @formal reason: completion projection.
//! PostgreSQL evidence loading for the shared Run-dispatch identity decision.
//!
//! This module owns no identity policy: it translates the two existing rows
//! into [`StoredRunIdentity`] and delegates the decision to `dispatch`.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{DispatchCompletion, RunDispatch};
use sqlx::postgres::{PgConnection, PgPool, PgRow};
use sqlx::types::Json;
use sqlx::{Postgres, Row, Transaction};

use crate::dispatch::{DispatchError, RunIdentityDecision, StoredRunIdentity, decide_run_identity};

pub(crate) async fn load_run_identity_evidence(
    connection: &mut PgConnection,
    _prefix: &str,
    run_id: &RunId,
) -> Result<PgRow, DispatchError> {
    sqlx::query("SELECT d.run_id AS live_run_id, d.thread_id AS live_thread_id, \
         d.status, d.request, c.sequence, c.run_id, c.request_fingerprint, \
         c.thread_id, c.session_thread_id, c.cancel_requested, c.retained_request::TEXT AS retained_request_json FROM (SELECT $1::TEXT AS run_id) k \
         LEFT JOIN runtime_dispatch d ON d.run_id = k.run_id \
         LEFT JOIN runtime_dispatch_completion c ON c.run_id = k.run_id")
    .bind(&run_id.0)
    .fetch_one(connection)
    .await
    .map_err(store_error)
}

pub(crate) fn decode_live_identity(row: &PgRow) -> Result<Option<RunDispatch>, DispatchError> {
    row.try_get::<Option<String>, _>("live_run_id")
        .map_err(store_error)?
        .map(|_| decode_live_dispatch(row))
        .transpose()
}

pub(crate) fn decode_live_dispatch(row: &PgRow) -> Result<RunDispatch, DispatchError> {
    row.try_get::<Json<RunDispatch>, _>("request")
        .map(|Json(request)| request)
        .map_err(store_error)
}

pub(crate) fn decode_completed_identity(
    row: &PgRow,
) -> Result<Option<Option<String>>, DispatchError> {
    row.try_get::<Option<String>, _>("run_id")
        .map_err(store_error)?
        .map(|_| row.try_get("request_fingerprint").map_err(store_error))
        .transpose()
}

pub(crate) async fn exact_run_replay(
    tx: &mut Transaction<'_, Postgres>,
    prefix: &str,
    request: &RunDispatch,
) -> Result<bool, DispatchError> {
    let evidence = load_run_identity_evidence(tx, prefix, request.run_id()).await?;
    let live = decode_live_identity(&evidence)?;
    let completed = if live.is_none() {
        decode_completed_identity(&evidence)?
    } else {
        None
    };
    let stored = match (&live, &completed) {
        (Some(live), _) => StoredRunIdentity::Live(live),
        (None, Some(completed)) => StoredRunIdentity::Completed(completed.as_deref()),
        (None, None) => StoredRunIdentity::Absent,
    };
    decide_run_identity(stored, request).map(|decision| decision == RunIdentityDecision::Replay)
}

pub(crate) fn decode_dispatch_completion(row: &PgRow) -> Result<DispatchCompletion, DispatchError> {
    let sequence = row.try_get::<i64, _>("sequence").map_err(store_error)?;
    Ok(DispatchCompletion {
        sequence: u64::try_from(sequence).map_err(|_| {
            DispatchError::Rejected("persisted completion sequence is negative".to_string())
        })?,
        run_id: RunId(row.try_get("run_id").map_err(store_error)?),
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")
            .map_err(store_error)?
            .map(awaken_agent_contract::agent::thread::Id),
        session_thread_id: row
            .try_get::<Option<String>, _>("session_thread_id")
            .map_err(store_error)?
            .map(awaken_agent_contract::agent::thread::Id),
        request_fingerprint: row.try_get("request_fingerprint").map_err(store_error)?,
        cancellation_requested: row
            .try_get::<i64, _>("cancel_requested")
            .map_err(store_error)?
            != 0,
    })
}

/// Serialize absent-row admission for one caller-owned Run id. Row locks cannot
/// protect an identity before its first insert; the transaction-scoped advisory
/// lock closes that gap without creating another registry or durable table.
pub(crate) async fn lock_run_identity(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
) -> Result<(), DispatchError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map(|_| ())
        .map_err(store_error)
}

/// Serialize absent-row capacity decisions for one parent Session. Child Run ids
/// differ, so their ordinary identity locks alone cannot protect a shared
/// distinct-Thread bound. A domain-separated advisory key supplies that narrow
/// transaction fence without another table or registry.
pub(crate) async fn lock_session_child_admission(
    tx: &mut Transaction<'_, Postgres>,
    parent_thread_id: &str,
) -> Result<(), DispatchError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("awaken:session-child-admission:{parent_thread_id}"))
        .execute(&mut **tx)
        .await
        .map(|_| ())
        .map_err(store_error)
}

pub(crate) async fn load_completion_events(
    pool: &PgPool,
    _prefix: &str,
    after_sequence: u64,
    limit: usize,
) -> Result<Vec<DispatchCompletion>, DispatchError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let after_sequence = i64::try_from(after_sequence).map_err(|_| {
        DispatchError::Rejected("completion cursor exceeds BIGINT range".to_string())
    })?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = sqlx::query(
        "SELECT sequence, run_id, request_fingerprint, thread_id, session_thread_id, \
         cancel_requested FROM runtime_dispatch_completion \
         WHERE sequence > $1 ORDER BY sequence LIMIT $2",
    )
    .bind(after_sequence)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    rows.iter().map(decode_dispatch_completion).collect()
}

fn store_error(error: impl std::fmt::Display) -> DispatchError {
    DispatchError::unavailable(error)
}
