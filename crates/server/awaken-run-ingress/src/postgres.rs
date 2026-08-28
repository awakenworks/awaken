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
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use sqlx::{Postgres, Transaction};

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, ClaimEpochStorageRow, Claimed, CommitEpochGuard,
    ContinuationAdmission, CredentialRealizationReceipt, DispatchCompletion, DispatchError,
    DispatchOutcome, DispatchQueue, DispatchState, DispatchSummary, ExactClaimMode, Inbox, Lease,
    Outbox, PendingInput, PendingRecord, RunClaim, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, SubmitOptions, can_admit_attempt_credentials,
    classify_completed_session_run_reservation, classify_exact_claim_mode,
    classify_live_session_run_reservation, classify_session_run_reservation_activation,
    compile_attempt_credential_bindings, ensure_session_child_capacity,
    installed_worker_credential_capabilities, normalize_pending_millis,
    retry_exhaustion_evidence_is_eligible, session_child_parent, session_child_thread,
    validate_executable_dispatch_admission, validate_outbox_continuation,
    validate_session_resume_activity_transition, validate_session_resume_evidence,
    validate_session_resume_target, validate_session_run_reservation_request,
    validate_session_run_reservation_resolution, verify_credential_realization_receipt,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::postgres_helpers::{
    append_pending_transaction, current_worker_claim, idempotency_conflict, load_pending_input,
    retry_exhausted_candidate,
};
use crate::postgres_identity::{
    exact_run_replay, load_completion_events, lock_run_identity, lock_session_child_admission,
};
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, DispatchPlacement, LeaseLossReason, PlacementPolicy, WorkerAssignment,
    WorkerSnapshot, can_assign, can_claim_locally, durable_i64, durable_u64, next_claim_epoch,
    policy_selects_requester,
};
use awaken_run_ingress_contract::RunDispatch;

pub use crate::postgres_checkpoint::PostgresStreamCheckpointStore;

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
pub(super) const NS: &str = "runtime";

/// Shared PostgreSQL candidate predicate. A normal wake/fresh claim waits behind
/// every other Running or Awaiting Run on its Thread. Cancellation waits only
/// behind a Running peer so legacy multi-Awaiting rows can drain sequentially.
/// The candidate is excluded in both branches, allowing its own exact wake.
fn thread_available_for_claim(prefix: &str) -> String {
    format!(
        "((d.cancel_requested = 1 AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.run_id <> d.run_id \
         AND r.status IN ('running', 'reservation_running'))) \
         OR (d.cancel_requested = 0 AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.run_id <> d.run_id \
         AND r.status IN ('running', 'reservation_running', 'awaiting'))))"
    )
}

fn no_running_peer(prefix: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.run_id <> d.run_id \
         AND r.status IN ('running', 'reservation_running'))"
    )
}

/// A Postgres-backed dispatch store.
pub struct PostgresDispatchStore {
    pool: PgPool,
}

pub(super) async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&bundle)
        .await
        .map(|_| ())
        .map_err(|err| StoreError::Migrate(err.to_string()))
}

pub(super) async fn verify_schema(pool: &PgPool) -> Result<(), StoreError> {
    let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .verify_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))
}

/// The one PostgreSQL insertion kernel after Run identity and any command-
/// specific admission have been decided under the caller's transaction locks.
async fn insert_new_dispatch(
    tx: &mut Transaction<'_, Postgres>,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
) -> Result<(), DispatchError> {
    insert_dispatch_with_state(tx, prefix, request, options, "pending", None).await
}

/// The single PostgreSQL row insertion owner. A Session reservation changes
/// only the initial phase/deadline; identity, dedupe, and supersession retain
/// the ordinary enqueue transaction.
async fn insert_dispatch_with_state(
    tx: &mut Transaction<'_, Postgres>,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
    initial_status: &'static str,
    lease_until: Option<i64>,
) -> Result<(), DispatchError> {
    let mut epoch = 0i64;
    if options.supersede {
        let max: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT MAX(epoch) FROM {prefix}_dispatch WHERE thread_id = $1"
        ))
        .bind(&request.thread_id().0)
        .fetch_one(&mut **tx)
        .await
        .map_err(reject)?;
        epoch = crate::next_supersession_epoch(max.unwrap_or(0))?;
        sqlx::query(&format!(
            "UPDATE {prefix}_dispatch SET status = 'superseded', lease_owner = NULL, \
             lease_until = NULL WHERE thread_id = $1 AND status IN ('pending', 'awaiting') \
             AND cancel_requested = 0"
        ))
        .bind(&request.thread_id().0)
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    }

    let inserted = sqlx::query(&format!(
        "INSERT INTO {prefix}_dispatch \
         (run_id, thread_id, request, status, priority, epoch, dedupe_key, lease_until) \
         SELECT $1, $2, $3, $7, $4, $5, $6, $8 \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM {prefix}_dispatch WHERE dedupe_key = $6 AND status <> 'dead_letter') \
         AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch_completion WHERE run_id = $1) \
         ON CONFLICT (run_id) DO NOTHING"
    ))
    .bind(&request.run_id().0)
    .bind(&request.thread_id().0)
    .bind(Json(request))
    .bind(options.priority)
    .bind(epoch)
    .bind(options.dedupe_key.as_deref())
    .bind(initial_status)
    .bind(lease_until)
    .execute(&mut **tx)
    .await
    .map_err(reject)?
    .rows_affected();
    if inserted > 0 {
        return Ok(());
    }
    if exact_run_replay(tx, prefix, request).await? {
        return Ok(());
    }
    if let Some(dedupe_key) = options.dedupe_key.as_deref() {
        let deduped: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {prefix}_dispatch \
             WHERE dedupe_key = $1 AND status <> 'dead_letter')"
        ))
        .bind(dedupe_key)
        .fetch_one(&mut **tx)
        .await
        .map_err(reject)?;
        if deduped {
            return Ok(());
        }
    }
    Err(DispatchError::Rejected(format!(
        "dispatch `{}` was not inserted despite having no matching Run or dedupe identity",
        request.run_id().0
    )))
}

async fn admit_session_child(
    tx: &mut Transaction<'_, Postgres>,
    prefix: &str,
    request: &RunDispatch,
    admission: &SessionChildAdmission,
) -> Result<(), DispatchError> {
    let parent = session_child_parent(request)?.clone();
    lock_session_child_admission(tx, &parent.0).await?;
    let rows = sqlx::query(&format!("SELECT request FROM {prefix}_dispatch"))
        .fetch_all(&mut **tx)
        .await
        .map_err(reject)?;
    let mut known = Vec::new();
    for row in rows {
        let Json(stored): Json<RunDispatch> = row.try_get("request").map_err(reject)?;
        if let Some(thread) = session_child_thread(&stored, &parent) {
            known.push(thread.clone());
        }
    }
    let completed: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT thread_id FROM {prefix}_dispatch_completion \
         WHERE session_thread_id = $1 AND thread_id IS NOT NULL \
         AND thread_id <> session_thread_id"
    ))
    .bind(&parent.0)
    .fetch_all(&mut **tx)
    .await
    .map_err(reject)?;
    known.extend(completed.into_iter().map(ThreadId));
    ensure_session_child_capacity(request, admission, known)
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
        PostgresStreamCheckpointStore::from_pool(self.pool.clone())
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

    async fn lock_claim_epoch_in_status(
        &self,
        claim: &RunClaim,
        required_status: &'static str,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let p = NS;
        // Keep this transaction alive in the opaque guard. The explicit status
        // keeps repair authority separate from ordinary execution authority.
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current: Option<ClaimEpochStorageRow<Json<RunDispatch>>> = sqlx::query_as(&format!(
            "SELECT lease_epoch, lease_owner, lease_until, request, cancel_requested \
                 FROM {p}_dispatch WHERE run_id = $1 AND status = $2 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(required_status)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        let request = match current {
            Some((epoch, owner, expires_ms, Json(request), cancellation_requested))
                if durable_u64("dispatch lease epoch", epoch)? == claim.epoch
                    && owner.as_deref() == Some(&claim.owner) =>
            {
                expires_ms
                    .map(|expires_ms| {
                        durable_u64("dispatch lease expiry", expires_ms)
                            .map(|expires_ms| (request, expires_ms, cancellation_requested != 0))
                    })
                    .transpose()?
            }
            Some(_) | None => None,
        };
        Ok(
            request.map(|(request, expires_ms, cancellation_requested)| {
                CommitEpochGuard::new(tx, request, expires_ms, cancellation_requested)
            }),
        )
    }
}

include!("postgres/dispatch_queue.rs");
include!("postgres/message_ports.rs");
include!("postgres/claim.rs");
fn reject(err: sqlx::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
