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
    CasOutcome, Claimed, DispatchError, DispatchOutcome, Lease, MessageOutbox, PendingInbox,
    PendingInput, PendingRecord, RunDispatch, SubmitOptions,
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

    /// Run ids in a terminal-ish dispatch status (dead_letter, superseded), in
    /// enqueue order — backs the operational `dead_letters`/`superseded` queries.
    async fn run_ids_by_status(&self, status: &str) -> Result<Vec<RunId>, DispatchError> {
        let p = &self.prefix;
        let rows = sqlx::query(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE status = $1 ORDER BY created_at"
        ))
        .bind(status)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("run_id")
                    .map(RunId)
                    .map_err(reject)
            })
            .collect()
    }
}

#[async_trait]
impl RunDispatch for PostgresDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunExecutionRequest,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Supersession: take the highest epoch on the thread and mark its prior
        // pending/parked work superseded — the newest submission wins (ADR-0022).
        let mut epoch = 0i64;
        if options.supersede {
            let max: Option<i64> = sqlx::query_scalar(&format!(
                "SELECT MAX(epoch) FROM {p}_dispatch WHERE thread_id = $1"
            ))
            .bind(&request.thread_id().0)
            .fetch_one(&mut *tx)
            .await
            .map_err(reject)?;
            epoch = max.unwrap_or(0) + 1;
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'superseded', lease_owner = NULL, \
                 lease_until = NULL WHERE thread_id = $1 AND status IN ('pending', 'parked')"
            ))
            .bind(&request.thread_id().0)
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        }

        // Insert unless the run id exists, or a live (non-dead-letter) dispatch
        // already carries the same dedupe key. A NULL dedupe key never matches.
        sqlx::query(&format!(
            "INSERT INTO {p}_dispatch \
             (run_id, thread_id, request, status, priority, epoch, dedupe_key) \
             SELECT $1, $2, $3, 'pending', $4, $5, $6 \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM {p}_dispatch WHERE dedupe_key = $6 AND status <> 'dead_letter') \
             ON CONFLICT (run_id) DO NOTHING"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .bind(options.priority)
        .bind(epoch)
        .bind(options.dedupe_key.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
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
                 SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                 AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
             ORDER BY d.created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let fresh = format!(
            "SELECT run_id, request FROM {p}_dispatch \
             WHERE status = 'pending' ORDER BY priority DESC, created_at \
             FOR UPDATE SKIP LOCKED LIMIT 1"
        );

        // Track whether this is a recovery pick (an expired-lease running row),
        // which spends one crash-retry; a fresh or wake pick does not.
        let mut recovery_pick = true;
        let picked = match sqlx::query(&recovery)
            .bind(now_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?
        {
            Some(row) => Some(row),
            None => {
                recovery_pick = false;
                match sqlx::query(&wake)
                    .bind(now_ms as i64)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(reject)?
                {
                    Some(row) => Some(row),
                    None => sqlx::query(&fresh)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(reject)?,
                }
            }
        };

        let Some(row) = picked else {
            return Ok(None);
        };
        let run_id: String = row.try_get("run_id").map_err(reject)?;
        let Json(request): Json<RunExecutionRequest> = row.try_get("request").map_err(reject)?;

        let expires = now_ms + lease_ms;
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2, \
             attempt_count = attempt_count + $3 WHERE run_id = $4"
        ))
        .bind(owner)
        .bind(expires as i64)
        .bind(i64::from(recovery_pick))
        .bind(&run_id)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;

        // Hand the run's current pending input to the worker. It is not removed
        // here: settle removes exactly what the worker reports it consumed, so a
        // crash before settle leaves the input to be re-derived (ADR-0010).
        let rows = sqlx::query(&format!(
            "SELECT message_id, thread_id, correlation_id, result, available_at FROM {p}_pending \
             WHERE run_id = $1 AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at"
        ))
        .bind(&run_id)
        .bind(now_ms as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?;

        let mut pending = Vec::with_capacity(rows.len());
        for prow in rows {
            let message_id: String = prow.try_get("message_id").map_err(reject)?;
            let thread_id: String = prow.try_get("thread_id").map_err(reject)?;
            let correlation_id: String = prow.try_get("correlation_id").map_err(reject)?;
            let Json(result): Json<ResumeResult> = prow.try_get("result").map_err(reject)?;
            let available_at: Option<i64> = prow.try_get("available_at").map_err(reject)?;
            pending.push(PendingInput {
                message_id,
                run_id: RunId(run_id.clone()),
                thread_id: ThreadId(thread_id),
                correlation_id,
                available_at_ms: available_at.map(|t| t as u64),
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

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET lease_until = $1 \
             WHERE run_id = $2 AND status = 'running' AND lease_owner = $3"
        ))
        .bind((now_ms + lease_ms) as i64)
        .bind(&run_id.0)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
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
                // Drop the run's own pending and anything else consumed this
                // attempt (e.g. unbound idle-thread input, ADR-0021).
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE run_id = $1 OR message_id = ANY($2)"
                ))
                .bind(&run_id.0)
                .bind(consumed)
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
                     lease_until = NULL, attempt_count = 0 WHERE run_id = $1"
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

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'dead_letter', lease_owner = NULL, \
             lease_until = NULL, dead_lettered_at = $1 \
             WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < $1 \
             AND attempt_count >= $2"
        ))
        .bind(now_ms as i64)
        .bind(max_attempts as i64)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("dead_letter").await
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("superseded").await
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET status = 'pending', attempt_count = 0, lease_owner = NULL, \
             lease_until = NULL WHERE run_id = $1 AND status = 'dead_letter'"
        ))
        .bind(&run_id.0)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let thread: Option<String> = sqlx::query_scalar(&format!(
            "SELECT thread_id FROM {p}_dispatch \
             WHERE run_id = $1 AND status IN ('pending', 'parked')"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if thread.is_some() {
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
        tx.commit().await.map_err(reject)?;
        Ok(thread.map(ThreadId))
    }

    async fn parked_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let p = &self.prefix;
        let run: Option<String> = sqlx::query_scalar(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE thread_id = $1 AND status = 'parked' \
             ORDER BY created_at LIMIT 1"
        ))
        .bind(&thread_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        Ok(run.map(RunId))
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "DELETE FROM {p}_pending WHERE run_id IN \
             (SELECT run_id FROM {p}_dispatch WHERE status = 'dead_letter')"
        ))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let result = sqlx::query(&format!(
            "DELETE FROM {p}_dispatch WHERE status = 'dead_letter'"
        ))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        let p = &self.prefix;
        let cond = "status = 'dead_letter' AND dead_lettered_at IS NOT NULL \
                    AND dead_lettered_at <= $1";
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "DELETE FROM {p}_pending WHERE run_id IN \
             (SELECT run_id FROM {p}_dispatch WHERE {cond})"
        ))
        .bind(cutoff_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let result = sqlx::query(&format!("DELETE FROM {p}_dispatch WHERE {cond}"))
            .bind(cutoff_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }
}

#[async_trait]
impl PendingInbox for PostgresDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "INSERT INTO {p}_pending \
             (message_id, run_id, thread_id, correlation_id, result, available_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.thread_id.0)
        .bind(&input.correlation_id)
        .bind(Json(&input.result))
        .bind(input.available_at_ms.map(|t| t as i64))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let p = &self.prefix;
        let rows = sqlx::query(&format!(
            "SELECT message_id, run_id, correlation_id, result, revision, available_at \
             FROM {p}_pending WHERE thread_id = $1 ORDER BY created_at"
        ))
        .bind(&thread_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let message_id: String = row.try_get("message_id").map_err(reject)?;
            let run_id: String = row.try_get("run_id").map_err(reject)?;
            let correlation_id: String = row.try_get("correlation_id").map_err(reject)?;
            let Json(result): Json<ResumeResult> = row.try_get("result").map_err(reject)?;
            let revision: i64 = row.try_get("revision").map_err(reject)?;
            let available_at: Option<i64> = row.try_get("available_at").map_err(reject)?;
            records.push(PendingRecord {
                input: PendingInput {
                    message_id,
                    run_id: RunId(run_id),
                    thread_id: thread_id.clone(),
                    correlation_id,
                    available_at_ms: available_at.map(|t| t as u64),
                    result,
                },
                revision: revision as u64,
            });
        }
        Ok(records)
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let outcome = match current_revision(&mut tx, p, message_id).await? {
            None => CasOutcome::NotFound,
            Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
            Some(_) => {
                sqlx::query(&format!("DELETE FROM {p}_pending WHERE message_id = $1"))
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(reject)?;
                CasOutcome::Applied
            }
        };
        tx.commit().await.map_err(reject)?;
        Ok(outcome)
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let p = &self.prefix;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let outcome = match current_revision(&mut tx, p, message_id).await? {
            None => CasOutcome::NotFound,
            Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
            Some(_) => {
                sqlx::query(&format!(
                    "UPDATE {p}_pending SET result = $1, revision = revision + 1 \
                     WHERE message_id = $2"
                ))
                .bind(Json(&result))
                .bind(message_id)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
                CasOutcome::Applied
            }
        };
        tx.commit().await.map_err(reject)?;
        Ok(outcome)
    }
}

#[async_trait]
impl MessageOutbox for PostgresDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let p = &self.prefix;
        let result = sqlx::query(&format!(
            "INSERT INTO {p}_outbox (message_id, payload) VALUES ($1, $2) \
             ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(Json(&input))
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() > 0)
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        let p = &self.prefix;
        let staged = sqlx::query(&format!("SELECT message_id, payload FROM {p}_outbox"))
            .fetch_all(&self.pool)
            .await
            .map_err(reject)?;

        let mut relayed = 0;
        for row in staged {
            let message_id: String = row.try_get("message_id").map_err(reject)?;
            let Json(input): Json<PendingInput> = row.try_get("payload").map_err(reject)?;

            // One transaction per message: idempotent target append, then drop
            // the outbox row. A crash before the delete re-appends (a no-op).
            let mut tx = self.pool.begin().await.map_err(reject)?;
            sqlx::query(&format!(
                "INSERT INTO {p}_pending \
                 (message_id, run_id, thread_id, correlation_id, result, available_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (message_id) DO NOTHING"
            ))
            .bind(&input.message_id)
            .bind(&input.run_id.0)
            .bind(&input.thread_id.0)
            .bind(&input.correlation_id)
            .bind(Json(&input.result))
            .bind(input.available_at_ms.map(|t| t as i64))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
            sqlx::query(&format!("DELETE FROM {p}_outbox WHERE message_id = $1"))
                .bind(&message_id)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            tx.commit().await.map_err(reject)?;
            relayed += 1;
        }
        Ok(relayed)
    }
}

async fn current_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prefix: &str,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    let revision: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT revision FROM {prefix}_pending WHERE message_id = $1"
    ))
    .bind(message_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)?;
    Ok(revision.map(|r| r as u64))
}

fn reject(err: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
