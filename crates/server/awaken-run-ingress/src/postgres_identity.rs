//! PostgreSQL evidence loading for the shared Run-dispatch identity decision.
//!
//! This module owns no identity policy: it translates the two existing rows
//! into [`StoredRunIdentity`] and delegates the decision to `dispatch`.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_run_ingress_contract::{DispatchCompletion, RunDispatch};
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use sqlx::{Postgres, Row, Transaction};

use crate::dispatch::{DispatchError, RunIdentityDecision, StoredRunIdentity, decide_run_identity};

pub(crate) async fn exact_run_replay(
    tx: &mut Transaction<'_, Postgres>,
    prefix: &str,
    request: &RunDispatch,
) -> Result<bool, DispatchError> {
    let live = sqlx::query(&format!(
        "SELECT request FROM {prefix}_dispatch WHERE run_id = $1"
    ))
    .bind(&request.run_id().0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?
    .map(|row| row.try_get::<Json<RunDispatch>, _>("request"))
    .transpose()
    .map_err(store_error)?
    .map(|Json(request)| request);
    let completed = if live.is_none() {
        sqlx::query(&format!(
            "SELECT request_fingerprint FROM {prefix}_dispatch_completion WHERE run_id = $1"
        ))
        .bind(&request.run_id().0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_error)?
        .map(|row| row.try_get::<Option<String>, _>("request_fingerprint"))
        .transpose()
        .map_err(store_error)?
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
    prefix: &str,
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
    let rows = sqlx::query(&format!(
        "SELECT sequence, run_id, request_fingerprint, thread_id, session_thread_id \
         FROM {prefix}_dispatch_completion \
         WHERE sequence > $1 ORDER BY sequence LIMIT $2"
    ))
    .bind(after_sequence)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    rows.into_iter()
        .map(|row| {
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
            })
        })
        .collect()
}

fn store_error(error: impl std::fmt::Display) -> DispatchError {
    DispatchError::unavailable(error)
}
