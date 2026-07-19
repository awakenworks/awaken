//! Postgres durable implementation of the dispatch-store ports.
//!
//! Two tables back the two aggregates: `runtime_dispatch` is the run-dispatch
//! queue (one row per accepted run, carrying the serializable
//! [`RunDispatch`] and its claim/lease state) and `runtime_pending` is
//! the thread's pending input. Claim is a single transaction using
//! `FOR UPDATE SKIP LOCKED`, so concurrent workers each take a distinct run
//! (single owner per run) without a global lock. The claim policy — recover an
//! expired lease, then wake an awaiting run with pending input, then a fresh run —
//! matches [`MemoryDispatchStore`](crate::MemoryDispatchStore) exactly.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchState, DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim,
    SettleOutcome, SubmitOptions,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::{WorkerAssignment, WorkerSnapshot, can_assign};
use awaken_run_ingress_contract::RunDispatch;

/// Errors from constructing or migrating the dispatch store. Claim/settle-time
/// failures use the neutral [`DispatchError`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// The component namespace for this runtime's tables. One runtime is one
/// component, so all its tables (dispatch and commit) share this prefix; the
/// scoped migration ledger isolates it from any other component in the same
/// database. It is built in, not configured.
const NS: &str = "runtime";

/// Max Postgres connections for the dispatch pool: `AWAKEN_PG_MAX_CONNECTIONS` if
/// set, else `available_parallelism() + 8` — the pool must cover the served pool's
/// `available_parallelism()` concurrent claim/settle drives, above sqlx's default 10.
fn pg_max_connections() -> u32 {
    std::env::var("AWAKEN_PG_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4) as u32;
            cores + 8
        })
}

/// A Postgres-backed dispatch store.
pub struct PostgresDispatchStore {
    pool: PgPool,
}

impl PostgresDispatchStore {
    /// Connect and apply the dispatch-schema migrations.
    ///
    /// Sized to the process's drain concurrency, not sqlx's default of 10: the pool
    /// spawns `available_parallelism()` drain tasks that each hold a connection to
    /// claim/settle a run, so a default pool starves a concurrent burst and strands
    /// the queue. Overridable via `AWAKEN_PG_MAX_CONNECTIONS`.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(pg_max_connections())
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the dispatch migrations under the
    /// runtime namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self { pool })
    }

    /// A clone of the connection pool for a [`PgNotifyWake`](crate::wake::PgNotifyWake):
    /// the wake listener shares the dispatch store's database (a `pg_notify` fired
    /// inside the enqueue transaction reaches a peer's `LISTEN` on the same pool), so
    /// the served pool can wake cross-node with no extra infrastructure. Cloning a
    /// `PgPool` clones the handle, not the connections.
    pub fn wake_pool(&self) -> PgPool {
        self.pool.clone()
    }

    /// Run ids in a terminal-ish dispatch status (dead_letter, superseded), in
    /// enqueue order — backs the operational `dead_letters`/`superseded` queries.
    async fn run_ids_by_status(&self, status: &str) -> Result<Vec<RunId>, DispatchError> {
        let p = NS;
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
impl DispatchQueue for PostgresDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Supersession: take the highest epoch on the thread and mark its prior
        // pending/awaiting work superseded — the newest submission wins (ADR-0022).
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
                 lease_until = NULL WHERE thread_id = $1 AND status IN ('pending', 'awaiting')"
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

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let expires = now_ms + lease_ms;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let inserted = sqlx::query_scalar::<_, i64>(&format!(
            "INSERT INTO {p}_dispatch \
             (run_id, thread_id, request, status, lease_owner, lease_until, lease_epoch) \
             VALUES ($1,$2,$3,'running',$4,$5,1) \
             ON CONFLICT (run_id) DO NOTHING RETURNING lease_epoch"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .bind(owner)
        .bind(expires as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if let Some(epoch) = inserted {
            let run_id = request.run_id().clone();
            tx.commit().await.map_err(reject)?;
            return Ok(Some(Claimed {
                request,
                lease: Lease {
                    run_id,
                    owner: owner.to_string(),
                    expires_ms: expires,
                    epoch: epoch as u64,
                },
                pending: Vec::new(),
                recovered: false,
                sandbox: None,
                assignment: None,
            }));
        }

        // Idempotent retries share the exact-claim transition kernel.
        let run_id = request.run_id().clone();
        let claimed =
            claim_exact_transaction(&mut tx, &run_id, owner, lease_ms, now_ms, None).await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        if can_assign(worker, &request.placement, None, false, now_ms).is_err() {
            return Ok(None);
        }
        let p = NS;
        let expires = now_ms + lease_ms;
        let assignment = WorkerAssignment::from(worker);
        let owner = worker.identity.lease_owner();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let inserted = sqlx::query_scalar::<_, i64>(&format!(
            "INSERT INTO {p}_dispatch \
             (run_id, thread_id, request, status, lease_owner, lease_until, lease_epoch, worker_assignment) \
             VALUES ($1,$2,$3,'running',$4,$5,1,$6) \
             ON CONFLICT (run_id) DO NOTHING RETURNING lease_epoch"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .bind(&owner)
        .bind(expires as i64)
        .bind(Json(&assignment))
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if let Some(epoch) = inserted {
            let run_id = request.run_id().clone();
            tx.commit().await.map_err(reject)?;
            return Ok(Some(Claimed {
                request,
                lease: Lease {
                    run_id,
                    owner,
                    expires_ms: expires,
                    epoch: epoch as u64,
                },
                pending: Vec::new(),
                recovered: false,
                sandbox: None,
                assignment: Some(assignment),
            }));
        }
        let run_id = request.run_id().clone();
        let claimed =
            claim_exact_transaction(&mut tx, &run_id, &owner, lease_ms, now_ms, Some(worker))
                .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_pending \
             (message_id, run_id, thread_id, correlation_id, result, available_at) \
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.thread_id.0)
        .bind(&input.correlation_id)
        .bind(Json(&input.result))
        .bind(input.available_at_ms.map(|time| time as i64))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let claimed =
            claim_exact_transaction(&mut tx, &run_id, owner, lease_ms, now_ms, None).await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_pending \
             (message_id, run_id, thread_id, correlation_id, result, available_at) \
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (message_id) DO NOTHING"
        ))
        .bind(&input.message_id)
        .bind(&input.run_id.0)
        .bind(&input.thread_id.0)
        .bind(&input.correlation_id)
        .bind(Json(&input.result))
        .bind(input.available_at_ms.map(|time| time as i64))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let p = NS;
        // Keep this transaction alive in the opaque guard. `FOR UPDATE` prevents
        // claim/reclaim/settle/cancel from changing or removing the authority row
        // until the real ThreadCommit on its own connection has completed.
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current: Option<(i64, Option<String>)> = sqlx::query_as(&format!(
            "SELECT lease_epoch, lease_owner FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        Ok(current
            .is_some_and(|(epoch, owner)| {
                epoch.max(0) as u64 == claim.epoch && owner.as_deref() == Some(&claim.owner)
            })
            .then(|| CommitEpochGuard::new(tx)))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Priority: recover an expired lease, then wake an awaiting run with pending
        // input, then a fresh pending run. Each locks its row, skipping rows a
        // concurrent worker already holds.
        let recovery = format!(
            "SELECT run_id, request, sandbox FROM {p}_dispatch \
             WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < $1 \
             ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        // Single-writer-per-thread (ADR-0022): a wake or fresh pick skips any thread
        // that already has a run in flight. Recovery is exempt (it re-owns the SAME
        // running row). The V0012 partial-unique index is the hard backstop under a
        // concurrent-claimer race that this SELECT guard cannot see (read-committed).
        let not_running = format!(
            "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
             WHERE r.thread_id = d.thread_id AND r.status = 'running')"
        );
        let wake = format!(
            "SELECT d.run_id, d.request, d.sandbox FROM {p}_dispatch d \
             WHERE d.status = 'awaiting' AND EXISTS ( \
                 SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                 AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
             AND {not_running} \
             ORDER BY d.created_at FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let fresh = format!(
            "SELECT d.run_id, d.request, d.sandbox FROM {p}_dispatch d \
             WHERE d.status = 'pending' AND {not_running} \
             ORDER BY d.priority DESC, d.created_at \
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
        let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
        let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;

        let expires = now_ms + lease_ms;
        // Bump the fence token on every claim (fresh, wake, recovery) and read it
        // back, so the returned lease carries the epoch the holder settles under.
        let claimed = sqlx::query_scalar::<_, i64>(&format!(
            "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2, \
             attempt_count = attempt_count + $3, lease_epoch = lease_epoch + 1, \
             worker_assignment = NULL \
             WHERE run_id = $4 RETURNING lease_epoch"
        ))
        .bind(owner)
        .bind(expires as i64)
        .bind(i64::from(recovery_pick))
        .bind(&run_id)
        .fetch_one(&mut *tx)
        .await;
        // The V0012 one-running-per-thread unique index is the topology-independent
        // backstop: if a concurrent claimer already made another run of this thread
        // running, this UPDATE hits the unique violation. That claim simply lost the
        // race — roll back and report "nothing claimed", the pool retries next tick.
        let lease_epoch = match claimed {
            Ok(epoch) => epoch,
            Err(err) => {
                if err.as_database_error().and_then(|e| e.code()).as_deref() == Some("23505") {
                    let _ = tx.rollback().await;
                    return Ok(None);
                }
                return Err(reject(err));
            }
        };

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
            sandbox,
            lease: Lease {
                run_id: RunId(run_id),
                owner: owner.to_string(),
                expires_ms: expires,
                epoch: lease_epoch as u64,
            },
            pending,
            recovered: recovery_pick,
            assignment: None,
        }))
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment FROM {p}_dispatch d WHERE \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
               AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
             (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
               WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
             ORDER BY CASE WHEN d.status = 'running' THEN 0 WHEN d.status = 'awaiting' THEN 1 ELSE 2 END, \
                      d.priority DESC, d.created_at"
        ))
        .bind(now_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let mut selected = None;
        for row in rows {
            let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(reject)?;
            if can_assign(
                worker,
                &request.placement,
                previous.as_ref().map(|value| &value.0),
                sandbox.is_some(),
                now_ms,
            )
            .is_ok()
            {
                selected = Some(RunId(row.try_get("run_id").map_err(reject)?));
                break;
            }
        }
        let Some(run_id) = selected else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let not_running = format!(
            "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
             WHERE r.thread_id = d.thread_id AND r.status = 'running')"
        );
        let sql = format!(
            "SELECT d.run_id, d.request, d.sandbox, d.status FROM {p}_dispatch d \
             WHERE d.run_id = $1 AND ( \
               (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
               OR (d.status = 'awaiting' AND EXISTS ( \
                 SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                 AND (pe.available_at IS NULL OR pe.available_at <= $2)) AND {not_running}) \
               OR (d.status = 'pending' AND {not_running}) \
             ) FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let Some(row) = sqlx::query(&sql)
            .bind(&requested_run.0)
            .bind(now_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?
        else {
            return Ok(None);
        };
        let run_id: String = row.try_get("run_id").map_err(reject)?;
        let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
        let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
        let status: String = row.try_get("status").map_err(reject)?;
        let recovery = status == "running";
        let expires = now_ms + lease_ms;
        let claimed = sqlx::query_scalar::<_, i64>(&format!(
            "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2, \
             attempt_count = attempt_count + $3, lease_epoch = lease_epoch + 1, \
             worker_assignment = NULL \
             WHERE run_id = $4 RETURNING lease_epoch"
        ))
        .bind(owner)
        .bind(expires as i64)
        .bind(i64::from(recovery))
        .bind(&run_id)
        .fetch_one(&mut *tx)
        .await;
        let lease_epoch = match claimed {
            Ok(epoch) => epoch,
            Err(error) => {
                if error
                    .as_database_error()
                    .and_then(|database| database.code())
                    .as_deref()
                    == Some("23505")
                {
                    let _ = tx.rollback().await;
                    return Ok(None);
                }
                return Err(reject(error));
            }
        };
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
        for row in rows {
            let message_id: String = row.try_get("message_id").map_err(reject)?;
            let thread_id: String = row.try_get("thread_id").map_err(reject)?;
            let correlation_id: String = row.try_get("correlation_id").map_err(reject)?;
            let Json(result): Json<ResumeResult> = row.try_get("result").map_err(reject)?;
            let available_at: Option<i64> = row.try_get("available_at").map_err(reject)?;
            pending.push(PendingInput {
                message_id,
                run_id: RunId(run_id.clone()),
                thread_id: ThreadId(thread_id),
                correlation_id,
                available_at_ms: available_at.map(|time| time as u64),
                result,
            });
        }
        tx.commit().await.map_err(reject)?;
        Ok(Some(Claimed {
            request,
            sandbox,
            lease: Lease {
                run_id: RunId(run_id),
                owner: owner.to_string(),
                expires_ms: expires,
                epoch: lease_epoch as u64,
            },
            pending,
            recovered: status == "running",
            assignment: None,
        }))
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            requested_run,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let p = NS;
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

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET sandbox = $1 WHERE run_id = $2 \
             AND status = 'running' AND lease_owner = $3 AND lease_epoch = $4"
        ))
        .bind(sandbox_ref)
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim.epoch as i64)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(if result.rows_affected() == 1 {
            SettleOutcome::Applied
        } else {
            SettleOutcome::Fenced
        })
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        let p = NS;
        let depth: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
             d.status = 'pending' OR \
             (d.status = 'running' AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND EXISTS (\
               SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
               AND (i.available_at IS NULL OR i.available_at <= $1)\
             ))"
        ))
        .bind(now_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(reject)?;
        Ok(Some(depth as u64))
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let p = NS;
        // Only rows within half a lease of expiring — a fresh claim's lease is a
        // full length out, so it is skipped until it approaches expiry, bounding
        // the heartbeat's write amplification (ADR-0024).
        let result = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET lease_until = $1 \
             WHERE status = 'running' AND lease_owner = $2 \
             AND lease_until IS NOT NULL AND lease_until < $3"
        ))
        .bind((now_ms + lease_ms) as i64)
        .bind(owner)
        .bind((now_ms + lease_ms / 2) as i64)
        .execute(&self.pool)
        .await
        .map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        // Fence first: mutate the dispatch row ONLY while the caller still holds the
        // current epoch. A stale owner (lower epoch) affects zero rows, so its settle
        // touches neither the dispatch nor its pending — the reclaimer's in-flight
        // state is inviolate.
        let dispatch_rows = match outcome {
            DispatchOutcome::Done => sqlx::query(&format!(
                "DELETE FROM {p}_dispatch WHERE run_id = $1 AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
            DispatchOutcome::Awaiting => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'awaiting', lease_owner = NULL, \
                 lease_until = NULL, attempt_count = 0 WHERE run_id = $1 AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
        };
        if dispatch_rows == 0 {
            // Fenced: the run was re-claimed under a higher epoch (or already gone).
            // Change nothing and report the loss so the stale caller abandons.
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        // The fence held; now reconcile the run's pending input.
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
            }
            DispatchOutcome::Awaiting => {
                sqlx::query(&format!(
                    "DELETE FROM {p}_pending WHERE message_id = ANY($1)"
                ))
                .bind(consumed)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
            }
        }
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let p = NS;
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

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT run_id, thread_id, status, attempt_count FROM {p}_dispatch \
             ORDER BY created_at"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|row| {
                Ok(DispatchSummary {
                    run_id: RunId(row.try_get("run_id").map_err(reject)?),
                    thread_id: ThreadId(row.try_get("thread_id").map_err(reject)?),
                    state: {
                        let status = row.try_get::<String, _>("status").map_err(reject)?;
                        DispatchState::from_db(&status).ok_or_else(|| {
                            DispatchError::Rejected(format!(
                                "unknown persisted dispatch state {status}"
                            ))
                        })?
                    },
                    attempt_count: row.try_get::<i64, _>("attempt_count").map_err(reject)? as u64,
                })
            })
            .collect()
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let p = NS;
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
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let thread: Option<String> = sqlx::query_scalar(&format!(
            "SELECT thread_id FROM {p}_dispatch \
             WHERE run_id = $1 AND status IN ('pending', 'awaiting')"
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

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let p = NS;
        let run: Option<String> = sqlx::query_scalar(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE thread_id = $1 AND status = 'awaiting' \
             ORDER BY created_at LIMIT 1"
        ))
        .bind(&thread_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?;
        Ok(run.map(RunId))
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        let p = NS;
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
        let p = NS;
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
impl Inbox for PostgresDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let p = NS;
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
        let p = NS;
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
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let outcome = match current_revision(&mut tx, message_id).await? {
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
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let outcome = match current_revision(&mut tx, message_id).await? {
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
impl Outbox for PostgresDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let p = NS;
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
        let p = NS;
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
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    let revision: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT revision FROM {NS}_pending WHERE message_id = $1"
    ))
    .bind(message_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)?;
    Ok(revision.map(|r| r as u64))
}

async fn claim_exact_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
) -> Result<Option<Claimed>, DispatchError> {
    let p = NS;
    let not_running = format!(
        "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.status = 'running')"
    );
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment FROM {p}_dispatch d \
         WHERE d.run_id = $1 AND ( \
           (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
           OR (d.status = 'awaiting' AND EXISTS ( \
             SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
             AND (pe.available_at IS NULL OR pe.available_at <= $2)) AND {not_running}) \
           OR (d.status = 'pending' AND {not_running}) \
         ) FOR UPDATE SKIP LOCKED LIMIT 1"
    );
    let Some(row) = sqlx::query(&sql)
        .bind(&requested_run.0)
        .bind(now_ms as i64)
        .fetch_optional(&mut **tx)
        .await
        .map_err(reject)?
    else {
        return Ok(None);
    };
    let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
    let previous: Option<Json<WorkerAssignment>> =
        row.try_get("worker_assignment").map_err(reject)?;
    let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
    if let Some(worker) = worker
        && can_assign(
            worker,
            &request.placement,
            previous.as_ref().map(|value| &value.0),
            sandbox.is_some(),
            now_ms,
        )
        .is_err()
    {
        return Ok(None);
    }
    let status: String = row.try_get("status").map_err(reject)?;
    let expires = now_ms + lease_ms;
    let lease_epoch = sqlx::query_scalar::<_, i64>(&format!(
        "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2, \
         attempt_count = attempt_count + $3, lease_epoch = lease_epoch + 1 \
         , worker_assignment = $5 WHERE run_id = $4 RETURNING lease_epoch"
    ))
    .bind(owner)
    .bind(expires as i64)
    .bind(i64::from(status == "running"))
    .bind(&requested_run.0)
    .bind(worker.map(WorkerAssignment::from).map(Json))
    .fetch_one(&mut **tx)
    .await
    .map_err(reject)?;
    let rows = sqlx::query(&format!(
        "SELECT message_id, thread_id, correlation_id, result, available_at \
         FROM {p}_pending WHERE run_id = $1 \
         AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at"
    ))
    .bind(&requested_run.0)
    .bind(now_ms as i64)
    .fetch_all(&mut **tx)
    .await
    .map_err(reject)?;
    let mut pending = Vec::with_capacity(rows.len());
    for row in rows {
        let Json(result): Json<ResumeResult> = row.try_get("result").map_err(reject)?;
        pending.push(PendingInput {
            message_id: row.try_get("message_id").map_err(reject)?,
            run_id: requested_run.clone(),
            thread_id: ThreadId(row.try_get("thread_id").map_err(reject)?),
            correlation_id: row.try_get("correlation_id").map_err(reject)?,
            available_at_ms: row
                .try_get::<Option<i64>, _>("available_at")
                .map_err(reject)?
                .map(|time| time as u64),
            result,
        });
    }
    Ok(Some(Claimed {
        request,
        lease: Lease {
            run_id: requested_run.clone(),
            owner: owner.to_string(),
            expires_ms: expires,
            epoch: lease_epoch as u64,
        },
        pending,
        recovered: status == "running",
        sandbox,
        assignment: worker.map(WorkerAssignment::from),
    }))
}

fn reject(err: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
