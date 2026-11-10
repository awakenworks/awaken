// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
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
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

use crate::dispatch::{no_running_peer, thread_available_for_claim};
use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use futures_util::TryStreamExt;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use sqlx::{Postgres, Transaction};

use crate::dispatch_schema::{
    BUNDLE_ID, CONVERGED_BUNDLE_ID, selected_converged_dispatch_bundle, selected_dispatch_bundle,
};
use crate::postgres_helpers::{
    append_pending_transaction, current_worker_claim, idempotency_conflict, load_pending_input,
    retry_exhausted_candidate,
};
use crate::postgres_identity::{
    decode_completed_identity, decode_dispatch_completion, decode_live_identity, exact_run_replay,
    load_completion_events, load_run_identity_evidence, lock_run_identity,
    lock_session_child_admission,
};
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, DispatchPage, DispatchPlacement,
    LeaseLossReason, PlacementPolicy, WorkerAssignment, WorkerSnapshot, can_assign,
    can_claim_locally, durable_i64, durable_u64, next_claim_epoch, policy_selects_requester,
};
use crate::{
    dispatch::AttemptAdmission, dispatch::AttemptExecutionBinding, dispatch::CasOutcome,
    dispatch::ClaimEpochStorageRow, dispatch::Claimed, dispatch::CommitEpochGuard,
    dispatch::ContinuationAdmission, dispatch::DispatchCompletion, dispatch::DispatchDepths,
    dispatch::DispatchError, dispatch::DispatchLease, dispatch::DispatchOutcome,
    dispatch::DispatchState, dispatch::DispatchSummary, dispatch::ExactClaimMode, dispatch::Inbox,
    dispatch::LeaseRenewalOutcome, dispatch::Outbox, dispatch::PendingInput,
    dispatch::PendingInputSettlementPlan, dispatch::PendingRecord, dispatch::PhysicalAttempt,
    dispatch::SessionChildAdmission, dispatch::SessionRunReservationActivation,
    dispatch::SessionRunReservationOutcome, dispatch::SessionRunReservationResolution,
    dispatch::SettleOutcome, dispatch::SubmitOptions, dispatch::can_admit_claim_credentials,
    dispatch::claim_credential_operation, dispatch::claim_with_frozen,
    dispatch::classify_completed_session_run_reservation, dispatch::classify_exact_claim_mode,
    dispatch::classify_live_session_run_reservation,
    dispatch::classify_session_run_reservation_activation,
    dispatch::compile_claim_credential_bindings, dispatch::decide_lease_renewal,
    dispatch::ensure_session_child_capacity, dispatch::installed_worker_credential_capabilities,
    dispatch::normalize_pending_millis, dispatch::pending_input_settlement_plan,
    dispatch::retry_exhaustion_evidence_is_eligible, dispatch::session_child_parent,
    dispatch::session_child_thread, dispatch::validate_executable_dispatch_admission,
    dispatch::validate_outbox_continuation, dispatch::validate_session_resume_activity_transition,
    dispatch::validate_session_resume_evidence, dispatch::validate_session_resume_target,
    dispatch::validate_session_run_replacement_candidates,
    dispatch::validate_session_run_reservation_request,
    dispatch::validate_session_run_reservation_resolution,
};
use awaken_credential_contract::{
    AttemptCredentialBinding, CredentialRealizationPersistenceDecision,
    CredentialRealizationReceipt, credential_realization_persistence_decision,
    verify_credential_realization_receipt,
};
use awaken_dispatch_contract::{
    CommitEpochAdmission, PhysicalAttemptSlot, PhysicalAttemptTransition, RunDispatch,
    classify_commit_epoch_admission,
};
use awaken_worker_contract::RunClaim;

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

async fn published_checksum(
    pool: &PgPool,
    bundle: &str,
    version: i64,
) -> Result<Option<String>, StoreError> {
    let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(pool)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    if ledger.is_none() {
        return Ok(None);
    }
    sqlx::query_scalar(&format!(
        "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = $1 AND version = $2"
    ))
    .bind(bundle)
    .bind(version)
    .fetch_optional(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))
}

/// A Postgres-backed dispatch store.
pub struct PostgresDispatchStore {
    pool: PgPool,
    /// Rows whose untrustworthy claim generation this store already reported,
    /// from a claim scan or the depth read.
    reported: crate::ReportedClaimGenerations,
}

pub(super) async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let published =
        selected_dispatch_bundle(published_checksum(pool, BUNDLE_ID, 15).await?.as_deref())
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
    let converged = selected_converged_dispatch_bundle(
        published_checksum(pool, CONVERGED_BUNDLE_ID, 7)
            .await?
            .as_deref(),
    )
    .map_err(|err| StoreError::Migrate(err.to_string()))?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
    runner
        .run_bundle(&published)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    runner
        .run_bundle(&converged)
        .await
        .map(|_| ())
        .map_err(|err| StoreError::Migrate(err.to_string()))
}

pub(super) async fn verify_schema(pool: &PgPool) -> Result<(), StoreError> {
    let published =
        selected_dispatch_bundle(published_checksum(pool, BUNDLE_ID, 15).await?.as_deref())
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
    let converged = selected_converged_dispatch_bundle(
        published_checksum(pool, CONVERGED_BUNDLE_ID, 7)
            .await?
            .as_deref(),
    )
    .map_err(|err| StoreError::Migrate(err.to_string()))?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
    runner
        .verify_bundle(&published)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    runner
        .verify_bundle(&converged)
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
        .map_err(store_error)?;
        epoch = crate::next_supersession_epoch(max.unwrap_or(0))?;
        let candidates = sqlx::query(&format!(
            "SELECT run_id, status, lease_epoch, cancel_requested FROM {prefix}_dispatch \
             WHERE thread_id = $1 FOR UPDATE"
        ))
        .bind(&request.thread_id().0)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_error)?;
        validate_session_run_replacement_candidates(
            request,
            candidates
                .iter()
                .map(|candidate| {
                    let status: String = candidate.try_get("status").map_err(store_error)?;
                    DispatchState::from_db(&status).ok_or_else(|| {
                        DispatchError::Rejected(format!(
                            "unknown persisted dispatch state `{status}`"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        for candidate in candidates {
            let run_id: String = candidate.try_get("run_id").map_err(store_error)?;
            let status: String = candidate.try_get("status").map_err(store_error)?;
            let lease_epoch: i64 = candidate.try_get("lease_epoch").map_err(store_error)?;
            let cancellation_requested: i64 =
                candidate.try_get("cancel_requested").map_err(store_error)?;
            let Some(next) = crate::persisted_dispatch_transition(
                &status,
                lease_epoch,
                cancellation_requested != 0,
            )?
            .supersede() else {
                continue;
            };
            sqlx::query(&format!(
                "UPDATE {prefix}_dispatch SET status = $2, lease_owner = NULL, \
                 lease_until = NULL WHERE run_id = $1"
            ))
            .bind(run_id)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut **tx)
            .await
            .map_err(store_error)?;
        }
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
    .map_err(store_error)?
    .rows_affected();
    if inserted > 0 {
        if let Some(scope) = &request.execution_scope {
            // The existing operational feed barrier also covers admissions.
            // Reuse its sequence so both read views freeze at one Dispatch head.
            sqlx::query(&format!(
                "LOCK TABLE {prefix}_dispatch_operation IN ROW EXCLUSIVE MODE"
            ))
            .execute(&mut **tx)
            .await
            .map_err(store_error)?;
            sqlx::query(&format!(
                "INSERT INTO {prefix}_dispatch_session_membership
                 (run_id, scope_id, session_id, thread_id, sequence, recorded_at_ms, initial_status, parent_run_id)
                 VALUES ($1, $2, $3, $4,
                   nextval(pg_get_serial_sequence($5, 'sequence')::regclass),
                   FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint, $6, $7)"
            ))
            .bind(&request.run_id().0)
            .bind(scope.0.as_str())
            .bind(&request.session_thread_id().0)
            .bind(&request.thread_id().0)
            .bind(format!("{prefix}_dispatch_operation"))
            .bind(initial_status)
            .bind(request.activation.delegation_origin.as_ref().map(|origin| &origin.parent_run_id.0))
            .execute(&mut **tx).await.map_err(store_error)?;
        }
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
        .map_err(store_error)?;
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
        .map_err(store_error)?;
    let mut known = Vec::new();
    for row in rows {
        let Json(stored): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
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
    .map_err(store_error)?;
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Connect to a dispatch schema already applied by the deployment migration
    /// phase. This path verifies the ledger and never executes DDL.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
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
        Ok(Self {
            pool,
            reported: crate::ReportedClaimGenerations::default(),
        })
    }

    /// Build from an existing pool after verifying the externally-owned ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        verify_schema(&pool).await?;
        Ok(Self {
            pool,
            reported: crate::ReportedClaimGenerations::default(),
        })
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
        .map_err(store_error)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("run_id")
                    .map(RunId)
                    .map_err(store_error)
            })
            .collect()
    }

    async fn lock_claim_epoch_in_status(
        &self,
        claim: &RunClaim,
        required_status: &'static str,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let request = lock_claim_row(&mut tx, claim, required_status).await?;
        Ok(request.map(
            |(request, expires_ms, cancellation_requested, physical_attempt)| {
                CommitEpochGuard::new_releasing(
                    async move { tx.rollback().await.map_err(|error| error.to_string()) },
                    request,
                    expires_ms,
                    cancellation_requested,
                    physical_attempt,
                )
            },
        ))
    }
}

/// Acquire the ordinary Run commit fence inside the transaction that will
/// publish committed truth. Reservation repair keeps its separate status gate.
pub async fn lock_running_claim_for_commit(
    tx: &mut Transaction<'_, Postgres>,
    claim: &RunClaim,
) -> Result<Option<RunDispatch>, DispatchError> {
    Ok(lock_claim_row(tx, claim, "running")
        .await?
        .map(|(request, _, _, _)| request))
}

async fn lock_claim_row(
    tx: &mut Transaction<'_, Postgres>,
    claim: &RunClaim,
    required_status: &str,
) -> Result<Option<(RunDispatch, u64, bool, PhysicalAttemptSlot)>, DispatchError> {
    let p = NS;
    let current: Option<ClaimEpochStorageRow<Json<RunDispatch>>> = sqlx::query_as(&format!(
            "SELECT lease_epoch, lease_owner, lease_until, request, cancel_requested, active_attempt_owner, active_attempt_epoch \
                 FROM {p}_dispatch WHERE run_id = $1 AND status = $2 FOR UPDATE"
        ))
        .bind(&claim.run_id.0)
        .bind(required_status)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_error)?;
    let request = match current {
        None => None,
        Some((
            epoch,
            owner,
            expires_ms,
            Json(request),
            cancellation_requested,
            attempt_owner,
            attempt_epoch,
        )) => {
            let epoch = crate::stored_claim_epoch(epoch)?;
            match classify_commit_epoch_admission(
                true,
                owner.is_some(),
                expires_ms.is_some(),
                owner.as_deref() == Some(&claim.owner),
                epoch == claim.epoch,
            ) {
                CommitEpochAdmission::Fenced => None,
                CommitEpochAdmission::Corrupt => {
                    return Err(DispatchError::Rejected(
                        "current dispatch claim has incomplete lease authority".to_string(),
                    ));
                }
                CommitEpochAdmission::Guarded => {
                    let expires_ms =
                        expires_ms.expect("guard admission requires a complete lease expiry");
                    Some((
                        request,
                        durable_u64("dispatch lease expiry", expires_ms)?,
                        cancellation_requested != 0,
                        awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
                            attempt_owner.is_some(),
                            attempt_epoch.is_some(),
                            attempt_owner.as_deref() == Some(&claim.owner),
                            attempt_epoch.and_then(|epoch| u64::try_from(epoch).ok())
                                == Some(claim.epoch),
                        ),
                    ))
                }
            }
        }
    };
    Ok(request)
}

include!("postgres/release_claim.rs");
include!("postgres/dispatch_queue.rs");
include!("postgres/summaries.rs");
include!("postgres/message_ports.rs");
include!("postgres/claim.rs");
include!("postgres/integrity.rs");
fn store_error(err: sqlx::Error) -> DispatchError {
    DispatchError::unavailable(err)
}
