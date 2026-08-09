//! Postgres durable implementation of the dispatch-store ports.
//!
//! Two tables back the two aggregates: `runtime_dispatch` is the run-dispatch
//! queue (one row per accepted run, carrying the serializable
//! [`RunDispatch`] and its claim/lease state) and `runtime_pending` is
//! the thread's pending input. Claim is a single transaction using
//! `FOR UPDATE SKIP LOCKED`, so concurrent workers each take a distinct run
//! (single owner per run) without a global lock. The claim policy — recover an
//! expired lease, then wake an awaiting run with pending input, then a fresh run —
//! matches the test-support `MemoryDispatchStore` reference backend exactly.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use sqlx::{Executor, Postgres, Row};

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, Claimed, CommitEpochGuard, CredentialRealizationReceipt,
    DispatchCompletion, DispatchError, DispatchOutcome, DispatchQueue, DispatchState,
    DispatchSummary, ExactClaimMode, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim,
    SettleOutcome, SubmitOptions, can_admit_attempt_credentials,
    compile_attempt_credential_bindings, installed_worker_credential_capabilities,
    normalize_pending_millis, verify_credential_realization_receipt,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, DispatchPlacement, LeaseLossReason, PlacementPolicy, WorkerAssignment,
    WorkerSnapshot, can_assign, can_claim_locally, policy_selects_requester,
};
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

/// A Postgres-backed dispatch store.
pub struct PostgresDispatchStore {
    pool: PgPool,
}

/// Best-effort PostgreSQL storage for an interrupted inference stream.
///
/// The dispatch authority holds the claim epoch lock while this adapter is
/// called. Keeping the mutable checkpoint in the same scoped runtime schema
/// makes it survive coordinator and worker replacement without adding another
/// database contract.
pub struct PostgresStreamCheckpointStore {
    pool: PgPool,
}

async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&bundle)
        .await
        .map(|_| ())
        .map_err(|err| StoreError::Migrate(err.to_string()))
}

async fn verify_schema(pool: &PgPool) -> Result<(), StoreError> {
    let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .verify_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))
}

impl PostgresDispatchStore {
    /// Connect and apply the dispatch-schema migrations.
    ///
    /// `max_connections` is deployment policy resolved by the composition root.
    /// This adapter owns connection mechanics only and never reads process
    /// configuration.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Connect to a dispatch schema already applied by the deployment migration
    /// phase. This path verifies the ledger and never executes DDL.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool: apply the dispatch migrations under the
    /// runtime namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Build from an existing pool after verifying the externally-owned ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        verify_schema(&pool).await?;
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

    /// A checkpoint adapter over this exact dispatch pool. The dispatch store and
    /// interrupted-stream state therefore share one migrated runtime schema and
    /// cannot drift onto a process-local fallback.
    pub(crate) fn checkpoint_store(&self) -> PostgresStreamCheckpointStore {
        PostgresStreamCheckpointStore {
            pool: self.pool.clone(),
        }
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

impl PostgresStreamCheckpointStore {
    /// Connect and apply the shared runtime-dispatch migration bundle.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Connect to the already-migrated shared runtime-dispatch schema.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool after applying the shared runtime schema.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Build from an existing pool after verifying the externally-owned ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        verify_schema(&pool).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl StreamCheckpointStore for PostgresStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint> {
        let row = sqlx::query(&format!(
            "SELECT checkpoint FROM {NS}_stream_checkpoint WHERE run_id = $1"
        ))
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .ok()??;
        row.try_get::<Json<StreamCheckpoint>, _>("checkpoint")
            .ok()
            .map(|value| value.0)
    }

    async fn put(&self, checkpoint: StreamCheckpoint) {
        let run_id = checkpoint.run_id.clone();
        let _ = sqlx::query(&format!(
            "INSERT INTO {NS}_stream_checkpoint (run_id, checkpoint) VALUES ($1, $2) \
             ON CONFLICT (run_id) DO UPDATE SET checkpoint = EXCLUDED.checkpoint"
        ))
        .bind(run_id)
        .bind(Json(checkpoint))
        .execute(&self.pool)
        .await;
    }

    async fn delete(&self, run_id: &str) {
        let _ = sqlx::query(&format!(
            "DELETE FROM {NS}_stream_checkpoint WHERE run_id = $1"
        ))
        .bind(run_id)
        .execute(&self.pool)
        .await;
    }
}

#[async_trait]
impl DispatchQueue for PostgresDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let p = NS;
        sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {p}_dispatch \
             WHERE run_id = $1 AND status = 'running' \
             AND lease_owner = $2 AND lease_until IS NOT NULL \
             AND lease_until >= $3)"
        ))
        .bind(&run_id.0)
        .bind(identity.lease_owner())
        .bind(crate::clock::db_millis(now_ms))
        .fetch_one(&self.pool)
        .await
        .map_err(reject)
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Run-id idempotency survives successful completion: a live row or the
        // permanent completion tombstone makes the whole command a no-op. Check
        // before supersession so replay cannot mutate sibling dispatches.
        let already_known: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM {p}_dispatch WHERE run_id = $1) OR \
             EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1)"
        ))
        .bind(&request.run_id().0)
        .fetch_one(&mut *tx)
        .await
        .map_err(reject)?;
        if already_known {
            tx.commit().await.map_err(reject)?;
            return Ok(());
        }

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
            epoch = crate::next_supersession_epoch(max.unwrap_or(0))?;
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'superseded', lease_owner = NULL, \
                 lease_until = NULL WHERE thread_id = $1 AND status IN ('pending', 'awaiting') \
                 AND cancel_requested = 0"
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
             AND NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1) \
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
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        if !can_claim_locally(&request.placement) {
            return Ok(None);
        }
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
             SELECT $1,$2,$3,'pending' \
             WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1) \
             ON CONFLICT (run_id) DO NOTHING"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
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
        let owner = worker.identity.lease_owner();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
             SELECT $1,$2,$3,'pending' \
             WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = $1) \
             ON CONFLICT (run_id) DO NOTHING"
        ))
        .bind(&request.run_id().0)
        .bind(&request.thread_id().0)
        .bind(Json(&request))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let run_id = request.run_id().clone();
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &owner,
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
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
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        append_pending_transaction(&mut tx, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
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
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.clone();
        let mut tx = self.pool.begin().await.map_err(reject)?;
        append_pending_transaction(&mut tx, &input).await?;
        let claimed = claim_exact_transaction(
            &mut tx,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
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
        let current: Option<(i64, Option<String>, Option<i64>, Json<RunDispatch>)> =
            sqlx::query_as(&format!(
                "SELECT lease_epoch, lease_owner, lease_until, request \
             FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
            ))
            .bind(&claim.run_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?;
        let request = current
            .filter(|(epoch, owner, _, _)| {
                (*epoch).max(0) as u64 == claim.epoch && owner.as_deref() == Some(&claim.owner)
            })
            .and_then(|(_, _, expires_ms, Json(request))| {
                expires_ms.map(|expires_ms| (request, expires_ms.max(0) as u64))
            });
        Ok(request.map(|(request, expires_ms)| CommitEpochGuard::new(tx, request, expires_ms)))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;

        // Priority chooses and locks a candidate only. `SKIP LOCKED` lets a
        // concurrent claimer consider the next eligible row instead of selecting
        // the same candidate and returning `None` at the exact transition below.
        // That transition remains the sole claim algorithm and rechecks every
        // cause while holding the row and per-thread transaction lock.
        let local_eligible = "(d.cancel_requested = 1 OR (\
            COALESCE(d.request #>> '{placement,location}', 'remote_preferred') \
                <> 'remote_required' AND \
            jsonb_array_length(COALESCE(\
                d.request #> '{placement,required_credentials}', '[]'::jsonb)) = 0))";
        let recovery = format!(
            "SELECT d.run_id FROM {p}_dispatch d \
             WHERE d.status = 'running' AND d.lease_until IS NOT NULL \
             AND d.lease_until < $1 AND {local_eligible} \
             ORDER BY d.cancel_requested DESC, d.created_at \
             FOR UPDATE SKIP LOCKED LIMIT 1"
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
            "SELECT d.run_id FROM {p}_dispatch d \
             WHERE d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
                 SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                 AND (pe.available_at IS NULL OR pe.available_at <= $1))) \
             AND {not_running} AND {local_eligible} \
             ORDER BY d.cancel_requested DESC, d.created_at \
             FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let fresh = format!(
            "SELECT d.run_id FROM {p}_dispatch d \
             WHERE d.status = 'pending' AND {not_running} AND {local_eligible} \
             ORDER BY d.cancel_requested DESC, d.priority DESC, d.created_at \
             FOR UPDATE SKIP LOCKED LIMIT 1"
        );
        let picked = match sqlx::query_scalar::<_, String>(&recovery)
            .bind(crate::clock::db_millis(now_ms))
            .fetch_optional(&mut *tx)
            .await
            .map_err(reject)?
        {
            Some(row) => Some(row),
            None => match sqlx::query_scalar::<_, String>(&wake)
                .bind(crate::clock::db_millis(now_ms))
                .fetch_optional(&mut *tx)
                .await
                .map_err(reject)?
            {
                Some(row) => Some(row),
                None => sqlx::query_scalar::<_, String>(&fresh)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(reject)?,
            },
        };
        let Some(run_id) = picked else {
            return Ok(None);
        };
        let claimed = claim_exact_transaction(
            &mut tx,
            &RunId(run_id),
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
               ) AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
             (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
               WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
             ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                      d.priority DESC, d.created_at"
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let capabilities = installed_worker_credential_capabilities(worker)?;
        let mut selected = None;
        for row in rows {
            let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(reject)?;
            let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(reject)?;
            let lease_epoch: i64 = row.try_get("lease_epoch").map_err(reject)?;
            if cancellation_requested != 0
                || (can_assign(
                    worker,
                    &request.placement,
                    previous.as_ref().map(|value| &value.0),
                    sandbox.is_some(),
                    now_ms,
                )
                .is_ok()
                    && (lease_epoch.max(0) as u64)
                        .checked_add(1)
                        .is_some_and(|epoch| {
                            can_admit_attempt_credentials(&request, &capabilities, epoch, now_ms)
                        }))
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
            &capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
               WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= $1)) \
               ) AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
             (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
               WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
             ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                      d.priority DESC, d.created_at"
        ))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let capabilities = installed_worker_credential_capabilities(requester)?;
        let mut selected = None;
        for row in rows {
            let Json(request): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
            let sandbox: Option<String> = row.try_get("sandbox").map_err(reject)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(reject)?;
            let status: String = row.try_get("status").map_err(reject)?;
            let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(reject)?;
            let lease_epoch: i64 = row.try_get("lease_epoch").map_err(reject)?;
            if cancellation_requested != 0
                || (policy_selects_requester(
                    &request,
                    policy.as_ref(),
                    DispatchPlacement {
                        recovered: status == "running",
                        previous: previous.as_ref().map(|value| &value.0),
                        sandbox_bound: sandbox.is_some(),
                        requester: &requester.identity,
                        workers: &workers,
                        now_ms,
                    },
                )? && (lease_epoch.max(0) as u64)
                    .checked_add(1)
                    .is_some_and(|epoch| {
                        can_admit_attempt_credentials(&request, &capabilities, epoch, now_ms)
                    }))
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
            &requester.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(requester),
            &capabilities,
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
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let claimed = claim_exact_transaction_with_mode(
            &mut tx,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            &Default::default(),
            ExactClaimMode::TerminalRecovery,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        Ok(claimed)
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
            &installed_worker_credential_capabilities(worker)?,
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
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms, lease_ms,
        )))
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

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current = sqlx::query(&format!(
            "SELECT credential_bindings, credential_receipts FROM {p}_dispatch \
             WHERE run_id = $1 AND status = 'running' \
             AND lease_owner = $2 AND lease_epoch = $3 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim.epoch as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(current) = current else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let bindings = current
            .try_get::<Option<Json<Vec<AttemptCredentialBinding>>>, _>("credential_bindings")
            .map_err(reject)?
            .map(|value| value.0)
            .unwrap_or_default();
        verify_credential_realization_receipt(&bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        let mut receipts = current
            .try_get::<Option<Json<Vec<CredentialRealizationReceipt>>>, _>("credential_receipts")
            .map_err(reject)?
            .map(|value| value.0)
            .unwrap_or_default();
        if let Some(existing) = receipts
            .iter()
            .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint)
        {
            if existing != &receipt {
                return Err(DispatchError::Rejected(
                    "credential realization receipt conflicts with committed evidence".to_string(),
                ));
            }
            tx.commit().await.map_err(reject)?;
            return Ok(SettleOutcome::Applied);
        }
        receipts.push(receipt);
        let changed = sqlx::query(&format!(
            "UPDATE {p}_dispatch SET credential_receipts = $1 \
             WHERE run_id = $2 AND status = 'running' \
             AND lease_owner = $3 AND lease_epoch = $4"
        ))
        .bind(Json(&receipts))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim.epoch as i64)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        if changed.rows_affected() != 1 {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        let p = NS;
        let depth: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
             d.status = 'pending' OR \
             (d.status = 'running' AND d.lease_until < $1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
               SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
               AND (i.available_at IS NULL OR i.available_at <= $1)\
             )))"
        ))
        .bind(crate::clock::db_millis(now_ms))
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
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms, lease_ms,
        )))
        .bind(owner)
        .bind(crate::clock::db_millis(crate::clock::deadline_millis(
            now_ms,
            lease_ms / 2,
        )))
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
        let owner = sqlx::query_scalar::<_, String>(&format!(
            "SELECT lease_owner FROM {p}_dispatch WHERE run_id = $1 \
             AND status = 'running' AND lease_epoch = $2 AND lease_owner IS NOT NULL \
             FOR UPDATE"
        ))
        .bind(&run_id.0)
        .bind(epoch as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(owner) = owner else {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        };
        let claim = RunClaim {
            run_id: run_id.clone(),
            owner,
            epoch,
        };
        // Fence first: mutate the dispatch row ONLY while the caller still holds the
        // current epoch. A stale owner (lower epoch) affects zero rows, so its settle
        // touches neither the dispatch nor its pending — the reclaimer's in-flight
        // state is inviolate.
        let dispatch_rows = match outcome {
            DispatchOutcome::Done => sqlx::query(&format!(
                "DELETE FROM {p}_dispatch WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
            ))
            .bind(&run_id.0)
            .bind(epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(reject)?
            .rows_affected(),
            DispatchOutcome::Awaiting => sqlx::query(&format!(
                "UPDATE {p}_dispatch SET status = 'awaiting', lease_owner = NULL, \
                 lease_until = NULL, attempt_count = 0 WHERE run_id = $1 \
                 AND status = 'running' AND lease_epoch = $2"
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
        if outcome == DispatchOutcome::Done {
            sqlx::query(&format!(
                "INSERT INTO {p}_dispatch_completion (run_id) VALUES ($1) \
                 ON CONFLICT (run_id) DO NOTHING"
            ))
            .bind(&run_id.0)
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
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
        insert_operation(&mut tx, &DispatchOperation::Settled { claim, outcome }).await?;
        tx.commit().await.map_err(reject)?;
        Ok(SettleOutcome::Applied)
    }

    async fn completion_events_after(
        &self,
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
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT sequence, run_id FROM {p}_dispatch_completion \
             WHERE sequence > $1 ORDER BY sequence LIMIT $2"
        ))
        .bind(after_sequence)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        rows.into_iter()
            .map(|row| {
                let sequence = row.try_get::<i64, _>("sequence").map_err(reject)?;
                Ok(DispatchCompletion {
                    sequence: u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted completion sequence is negative".to_string(),
                        )
                    })?,
                    run_id: RunId(row.try_get("run_id").map_err(reject)?),
                })
            })
            .collect()
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let rows = sqlx::query(&format!(
            "WITH candidates AS ( \
                 SELECT run_id, lease_owner, lease_epoch, attempt_count \
                 FROM {p}_dispatch WHERE status = 'running' \
                 AND lease_until IS NOT NULL AND lease_until < $1 \
                 AND attempt_count >= $2 FOR UPDATE \
             ) \
             UPDATE {p}_dispatch AS dispatch SET status = 'dead_letter', \
             lease_owner = NULL, lease_until = NULL, dead_lettered_at = $1 \
             FROM candidates WHERE dispatch.run_id = candidates.run_id \
             RETURNING dispatch.run_id, candidates.lease_owner, \
                       candidates.lease_epoch, candidates.attempt_count"
        ))
        .bind(crate::clock::db_millis(now_ms))
        .bind(max_attempts as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(reject)?;
        for row in &rows {
            let owner = row
                .try_get::<Option<String>, _>("lease_owner")
                .map_err(reject)?
                .ok_or_else(|| {
                    DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    )
                })?;
            let epoch = row.try_get::<i64, _>("lease_epoch").map_err(reject)?;
            let attempt_count = row.try_get::<i64, _>("attempt_count").map_err(reject)?;
            let claim = RunClaim {
                run_id: RunId(row.try_get("run_id").map_err(reject)?),
                owner,
                epoch: epoch.max(0) as u64,
            };
            insert_operation(
                &mut tx,
                &DispatchOperation::LeaseLost {
                    claim: claim.clone(),
                    reason: LeaseLossReason::RetryExhausted,
                },
            )
            .await?;
            insert_operation(
                &mut tx,
                &DispatchOperation::DeadLettered {
                    claim,
                    attempt_count: attempt_count.max(0) as u64,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(reject)?;
        Ok(rows.len())
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
            "SELECT run_id, thread_id, status, attempt_count, cancel_requested, sandbox FROM {p}_dispatch \
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
                    cancellation_requested: row
                        .try_get::<i64, _>("cancel_requested")
                        .map_err(reject)?
                        != 0,
                    attempt_count: row.try_get::<i64, _>("attempt_count").map_err(reject)? as u64,
                    sandbox_bound: row
                        .try_get::<Option<String>, _>("sandbox")
                        .map_err(reject)?
                        .is_some(),
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
        let current = sqlx::query(&format!(
            "SELECT thread_id, status, lease_owner, lease_epoch FROM {p}_dispatch \
             WHERE run_id = $1 AND status IN ('pending', 'awaiting', 'running') \
             FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let Some(current) = current else {
            tx.commit().await.map_err(reject)?;
            return Ok(None);
        };
        let thread = current.try_get::<String, _>("thread_id").map_err(reject)?;
        let status = current.try_get::<String, _>("status").map_err(reject)?;
        let previous_owner = current
            .try_get::<Option<String>, _>("lease_owner")
            .map_err(reject)?;
        let previous_epoch = current.try_get::<i64, _>("lease_epoch").map_err(reject)?;
        let next_epoch = if status == "running" {
            Some(
                i64::try_from(crate::next_claim_epoch(previous_epoch)?).map_err(|_| {
                    DispatchError::Rejected(
                        "dispatch claim epoch exceeds the Postgres authority range".to_string(),
                    )
                })?,
            )
        } else {
            None
        };
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET cancel_requested = 1, \
             lease_epoch = CASE WHEN status = 'running' THEN $2 ELSE lease_epoch END, \
             lease_owner = CASE WHEN status = 'running' THEN NULL ELSE lease_owner END, \
             lease_until = CASE WHEN status = 'running' THEN NULL ELSE lease_until END, \
             status = CASE WHEN status = 'running' THEN 'pending' ELSE status END \
             WHERE run_id = $1"
        ))
        .bind(&run_id.0)
        .bind(next_epoch)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        if status == "running" {
            insert_operation(
                &mut tx,
                &DispatchOperation::LeaseLost {
                    claim: RunClaim {
                        run_id: run_id.clone(),
                        owner: previous_owner.ok_or_else(|| {
                            DispatchError::Rejected(
                                "running dispatch has no persisted lease owner".to_string(),
                            )
                        })?,
                        epoch: u64::try_from(previous_epoch).map_err(|_| {
                            DispatchError::Rejected(
                                "persisted dispatch claim epoch is negative".to_string(),
                            )
                        })?,
                    },
                    reason: LeaseLossReason::Cancelled,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(reject)?;
        Ok(Some(ThreadId(thread)))
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let p = NS;
        let run: Option<String> = sqlx::query_scalar(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE thread_id = $1 AND status = 'awaiting' \
             AND cancel_requested = 0 \
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
        .bind(crate::clock::db_millis(cutoff_ms))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
        let result = sqlx::query(&format!("DELETE FROM {p}_dispatch WHERE {cond}"))
            .bind(crate::clock::db_millis(cutoff_ms))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        tx.commit().await.map_err(reject)?;
        Ok(result.rows_affected() as usize)
    }
}

#[async_trait]
impl DispatchOperationalFeed for PostgresDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError> {
        if limit == 0 {
            return Ok(DispatchPage {
                events: Vec::new(),
                next_cursor: cursor,
            });
        }
        let after = i64::try_from(cursor.0).map_err(|_| {
            DispatchError::Rejected("dispatch cursor exceeds BIGINT range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let prefix = NS;
        let rows = sqlx::query(&format!(
            "SELECT sequence, recorded_at_ms, operation FROM {prefix}_dispatch_operation \
             WHERE sequence > $1 ORDER BY sequence LIMIT $2"
        ))
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(reject)?;
        let events = rows
            .into_iter()
            .map(|row| {
                let sequence = row.try_get::<i64, _>("sequence").map_err(reject)?;
                let recorded_at_ms = row
                    .try_get::<Option<i64>, _>("recorded_at_ms")
                    .map_err(reject)?
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation time is negative".to_string(),
                        )
                    })?;
                let Json(operation): Json<DispatchOperation> =
                    row.try_get("operation").map_err(reject)?;
                Ok(DispatchOperationalEvent {
                    cursor: DispatchCursor(u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation sequence is negative".to_string(),
                        )
                    })?),
                    recorded_at_ms,
                    operation,
                })
            })
            .collect::<Result<Vec<_>, DispatchError>>()?;
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(DispatchPage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl Inbox for PostgresDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let inserted = append_pending_transaction(&mut tx, &input).await?;
        tx.commit().await.map_err(reject)?;
        Ok(inserted)
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
                    available_at_ms: available_at
                        .map(crate::clock::millis_from_db)
                        .transpose()
                        .map_err(|err| DispatchError::Rejected(err.to_string()))?,
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
        let input = normalize_pending_millis(input);
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
        if result.rows_affected() > 0 {
            return Ok(true);
        }
        let existing = sqlx::query(&format!(
            "SELECT payload FROM {p}_outbox WHERE message_id = $1"
        ))
        .bind(&input.message_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(reject)?
        .map(|row| {
            row.try_get::<Json<PendingInput>, _>("payload")
                .map(|value| value.0)
        })
        .transpose()
        .map_err(reject)?;
        match existing {
            Some(existing) if existing == input => Ok(false),
            Some(_) => Err(idempotency_conflict(&input.message_id, "outbox")),
            None => Err(DispatchError::Rejected(format!(
                "outbox `{}` vanished during idempotency validation",
                input.message_id
            ))),
        }
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
            let input = normalize_pending_millis(input);

            // One transaction per message: idempotent target append, then drop
            // the outbox row. A crash before the delete re-appends (a no-op).
            let mut tx = self.pool.begin().await.map_err(reject)?;
            append_pending_transaction(&mut tx, &input).await?;
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

async fn insert_operation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    operation: &DispatchOperation,
) -> Result<(), DispatchError> {
    let prefix = NS;
    let recorded_at_ms = i64::try_from(crate::clock::system_now_ms()).unwrap_or(i64::MAX);
    sqlx::query(&format!(
        "INSERT INTO {prefix}_dispatch_operation (run_id, operation, recorded_at_ms) \
         VALUES ($1, $2, $3)"
    ))
    .bind(&operation.run_id().0)
    .bind(Json(operation))
    .bind(recorded_at_ms)
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    Ok(())
}

async fn claim_exact_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    claim_exact_transaction_with_mode(
        tx,
        requested_run,
        owner,
        lease_ms,
        now_ms,
        worker,
        capabilities,
        ExactClaimMode::Runnable,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn claim_exact_transaction_with_mode(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let p = NS;
    let thread_id = sqlx::query_scalar::<_, String>(&format!(
        "SELECT thread_id FROM {p}_dispatch WHERE run_id = $1"
    ))
    .bind(&requested_run.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)?;
    let Some(thread_id) = thread_id else {
        return Ok(None);
    };
    // The partial unique index remains the hard backstop, while this transaction
    // lock orders all claim transitions for one Thread before the eligibility
    // recheck. Hash collisions only over-serialize unrelated Threads.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&thread_id)
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    let not_running = format!(
        "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.status = 'running')"
    );
    let eligibility = match mode {
        ExactClaimMode::Runnable => format!(
            "(d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
             OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
               AND (pe.available_at IS NULL OR pe.available_at <= $2))) AND {not_running}) \
             OR (d.status = 'pending' AND {not_running})"
        ),
        ExactClaimMode::TerminalRecovery => {
            format!(
                "(d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
                 OR (d.status = 'awaiting' AND d.lease_owner IS NULL AND d.lease_until IS NULL \
                 AND {not_running})"
            )
        }
    };
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch FROM {p}_dispatch d \
         WHERE d.run_id = $1 AND ({eligibility}) FOR UPDATE SKIP LOCKED LIMIT 1"
    );
    let Some(row) = sqlx::query(&sql)
        .bind(&requested_run.0)
        .bind(crate::clock::db_millis(now_ms))
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
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(reject)?;
    let previous_owner: Option<String> = row.try_get("lease_owner").map_err(reject)?;
    let previous_epoch: i64 = row.try_get("lease_epoch").map_err(reject)?;
    let terminal_recovery = mode == ExactClaimMode::TerminalRecovery;
    if !terminal_recovery
        && cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref().map(|value| &value.0),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let status: String = row.try_get("status").map_err(reject)?;
    let claim_epoch = crate::next_claim_epoch(previous_epoch)?;
    let credential_bindings = if terminal_recovery || cancellation_requested != 0 {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&request, capabilities, claim_epoch, now_ms).map_err(
            |error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            },
        )?
    };
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    sqlx::query(&format!(
        "UPDATE {p}_dispatch SET status = 'running', lease_owner = $1, lease_until = $2, \
         attempt_count = attempt_count + $3, lease_epoch = $5, worker_assignment = $6, \
         credential_bindings = $7, credential_receipts = $8 WHERE run_id = $4"
    ))
    .bind(owner)
    .bind(crate::clock::db_millis(expires))
    .bind(i64::from(status == "running"))
    .bind(&requested_run.0)
    .bind(i64::try_from(claim_epoch).map_err(|_| {
        DispatchError::Rejected(
            "dispatch claim epoch exceeds the Postgres authority range".to_string(),
        )
    })?)
    .bind(
        (!terminal_recovery)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten()
            .map(Json),
    )
    .bind(Json(&credential_bindings))
    .bind(Json(Vec::<CredentialRealizationReceipt>::new()))
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    let claim = RunClaim {
        run_id: requested_run.clone(),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if status == "running" {
        let previous = RunClaim {
            run_id: requested_run.clone(),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: u64::try_from(previous_epoch).map_err(|_| {
                DispatchError::Rejected("persisted dispatch claim epoch is negative".to_string())
            })?,
        };
        insert_operation(
            tx,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason: LeaseLossReason::Expired,
            },
        )
        .await?;
        insert_operation(
            tx,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
        )
        .await?;
    } else {
        insert_operation(
            tx,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
        )
        .await?;
    }
    let rows = sqlx::query(&format!(
        "SELECT message_id, thread_id, correlation_id, result, available_at \
         FROM {p}_pending WHERE run_id = $1 \
         AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at"
    ))
    .bind(&requested_run.0)
    .bind(crate::clock::db_millis(now_ms))
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
                .map(crate::clock::millis_from_db)
                .transpose()
                .map_err(|err| DispatchError::Rejected(err.to_string()))?,
            result,
        });
    }
    Ok(Some(Claimed {
        request,
        lease: Lease {
            run_id: claim.run_id,
            owner: claim.owner,
            expires_ms: expires,
            epoch: claim.epoch,
        },
        credential_bindings,
        cancellation_requested: cancellation_requested != 0,
        pending,
        recovered: status == "running",
        sandbox,
        assignment: (!terminal_recovery)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten(),
    }))
}

/// The one PostgreSQL pending insert path, reused by direct delivery, Inbox
/// append, and outbox relay so retry/conflict semantics stay transactional.
async fn append_pending_transaction(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let prefix = NS;
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

fn idempotency_conflict(message_id: &str, aggregate: &str) -> DispatchError {
    DispatchError::Rejected(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn reject(err: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
