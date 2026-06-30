//! Postgres durable implementation of the dispatch-store ports.
//!
//! Two tables back the two aggregates: `{prefix}_dispatch` is the run-dispatch
//! queue (one row per accepted run, carrying the serializable
//! [`RunExecutionRequest`] and its claim/lease state) and `{prefix}_pending` is
//! the thread's pending input. Claim is a single transaction using
//! `FOR UPDATE SKIP LOCKED`, so concurrent workers each take a distinct run
//! (single owner per run) without a global lock. The claim policy — recover an
//! expired lease, then wake a parked run with pending input, then a fresh run —
//! matches [`MemoryDispatchStore`](crate::MemoryDispatchStore) exactly.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::dispatch::{
    Claimed, DispatchError, DispatchOutcome, Lease, PendingInbox, PendingInput, RunDispatch,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::request::RunExecutionRequest;

/// Errors from constructing or migrating the dispatch store. Claim/settle-time
/// failures use the neutral [`DispatchError`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A Postgres-backed dispatch store.
pub struct PostgresDispatchStore {
    pool: PgPool,
    prefix: String,
}

impl PostgresDispatchStore {
    /// Connect and apply the dispatch-schema migrations.
    pub async fn connect(url: &str, prefix: impl Into<String>) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool, prefix).await
    }

    /// Build from an existing pool: apply migrations. `prefix` namespaces the
    /// tables so the dispatch and commit schemas can share one database.
    pub async fn with_pool(pool: PgPool, prefix: impl Into<String>) -> Result<Self, StoreError> {
        let prefix = prefix.into();
        let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            &prefix,
        )
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self { pool, prefix })
    }
}

#[async_trait]
impl RunDispatch for PostgresDispatchStore {
    async fn enqueue(&self, request: RunExecutionRequest) -> Result<(), DispatchError> {
        let p = &self.prefix;
        sqlx::query(&format!(
            "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
             VALUES ($1, $2, $3, 'pending') ON CONFLICT (run_id) DO NOTHING"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(())
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Priority: recover an expired lease, then wake a parked run with pending
        // input, then a fresh pending run. Each locks its row, skipping rows a
        // concurrent worker already holds.
        let recovery = format!(
            "SELECT run_id, request FROM {p}_dispatch \
             WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < $1 \
             ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let wake = format!(
            "SELECT d.run_id, d.request FROM {p}_dispatch d \
             WHERE d.status = 'parked' AND EXISTS ( \
                 SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id) \
             ORDER BY d.created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let fresh = format!(
            "SELECT run_id, request FROM {p}_dispatch \
             WHERE status = 'pending' ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );

        let picked = match sqlx::query(&recovery)
            .bind(now_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?
        {
            Some(row) => Some(row),
            None => match sqlx::query(&wake)
                .fetch_optional(&mut *tx)
                .await
                .map_err(reject)?
            {
                Some(row) => Some(row),
                None => sqlx::query(&fresh)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(reject)?,
            },
        };

        let Some(row) = picked else {
            return Ok(None);
        };
        let run_id: String = row.try_get("run_id").map_err(reject)?;
        let Json(request): Json<RunExecutionRequest> = row.try_get("request").map_err(reject)?;

        let expires = now_ms + lease_ms;
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2 \
             WHERE run_id = $3"
        ))
        .bind(owner)
        .bind(expires as i64)
        .bind(&run_id)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;

        // Hand the run's current pending input to the worker. It is not removed
        // here: settle removes exactly what the worker reports it consumed, so a
        // crash before settle leaves the input to be re-derived (ADR-0010).
        let rows = sqlx::query(&format!(
            "SELECT message_id, thread_id, correlation_id, result FROM {p}_pending \
             WHERE run_id = $1 ORDER BY created_at"
        ))
        .bind(&run_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?;

        let mut pending = Vec::with_capacity(rows.len());
        for prow in rows {
            let message_id: String = prow.try_get("message_id").map_err(reject)?;
            let thread_id: String = prow.try_get("thread_id").map_err(reject)?;
            let correlation_id: String = prow.try_get("correlation_id").map_err(reject)?;
            let Json(result): Json<ResumeResult> = prow.try_get("result").map_err(reject)?;
            pending.push(PendingInput {
                message_id,
                run_id: RunId(run_id.clone()),
                thread_id: ThreadId(thread_id),
                correlation_id,
                result,
            });
        }

        tx.commit().await.map_err(reject)?;

        Ok(Some(Claimed {
            request,
            lease: Lease {
                run_id: RunId(run_id),
                owner: owner.to_string(),
                expires_ms: expires,
            },
            pending,
        }))
    }

    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        match outcome {
            DispatchOutcome::Done => {
                sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = $1"))
                    .bind(&run_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(reject)?;
                sqlx::query(&format!("DELETE FROM {p}_dispatch WHERE run_id = $1"))
                    .bind(&run_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(reject)?;
            }
            DispatchOutcome::Parked => {
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE message_id = ANY($1)"
                ))
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
                sqlx::query(&format!(
                    "UPDATE {p}_dispatch SET status = 'parked', lease_owner = NULL, \
                     lease_until = NULL WHERE run_id = $1"
                ))
                .bind(&run_id.0)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
        }
        tx.commit().await.map_err(reject)?;
        Ok(())
    }
}

#[async_trait]
impl PendingInbox for PostgresDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "INSERT INTO {p}_pending (message_id, run_id, thread_id, correlation_id, result) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.thread_id.0)
        .bind(&input.correlation_id)
        .bind(Json(&input.result))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }
}

fn reject(err: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
