// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
//! SQLite durable implementation of the dispatch-store ports.
//!
//! The embedded sibling of [`PostgresDispatchStore`](crate::PostgresDispatchStore),
//! over the *same* dispatch schema. SQLite has no `FOR UPDATE SKIP LOCKED`, but it
//! does not need it: every claim runs in a `BEGIN IMMEDIATE` transaction that
//! takes the database write lock, so claims serialize and a run is owned by one
//! worker at a time. The claim policy — recover an expired lease, then wake a
//! awaiting run with pending input, then a fresh run — matches
//! the test-support `MemoryDispatchStore` reference backend exactly. Each
//! operation holds the store's one sqlx connection (ADR-0087) for its whole span.

use crate::dispatch::{no_running_peer, thread_available_for_claim};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use futures_util::TryStreamExt;
use sqlx::sqlite::{Sqlite, SqliteConnection};

use crate::dispatch::{
    AttemptAdmission, AttemptCredentialBinding, AttemptExecutionBinding, CasOutcome,
    ClaimEpochStorageRow, Claimed, CommitEpochGuard, ContinuationAdmission,
    CredentialRealizationPersistenceDecision, CredentialRealizationReceipt, DispatchCompletion,
    DispatchDepths, DispatchError, DispatchLease, DispatchOutcome, DispatchState, DispatchSummary,
    ExactClaimMode, Inbox, LeaseRenewalOutcome, Outbox, PendingInput, PendingInputSettlementPlan,
    PendingRecord, PhysicalAttempt, RunClaim, RunIdentityDecision, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, StoredRunIdentity, SubmitOptions, can_admit_claim_credentials,
    claim_credential_operation, claim_with_frozen, classify_completed_session_run_reservation,
    classify_exact_claim_mode, classify_live_session_run_reservation,
    classify_session_run_reservation_activation, compile_claim_credential_bindings,
    credential_realization_persistence_decision, decide_lease_renewal, decide_run_identity,
    ensure_session_child_capacity, installed_worker_credential_capabilities,
    normalize_pending_millis, pending_input_settlement_plan, retry_exhaustion_evidence_is_eligible,
    session_child_parent, session_child_thread, validate_executable_dispatch_admission,
    validate_outbox_continuation, validate_session_resume_activity_transition,
    validate_session_resume_evidence, validate_session_resume_target,
    validate_session_run_replacement_candidates, validate_session_run_reservation_request,
    validate_session_run_reservation_resolution, verify_credential_realization_receipt,
};
use crate::dispatch_schema::{
    BUNDLE_ID, CONVERGED_BUNDLE_ID, selected_converged_dispatch_bundle, selected_dispatch_bundle,
};
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, DispatchPlacement, LeaseLossReason, PlacementPolicy, WorkerAssignment,
    WorkerSnapshot, can_assign, can_claim_locally, durable_i64, durable_u64, next_claim_epoch,
    policy_selects_requester,
};
use awaken_dispatch_contract::{
    CommitEpochAdmission, PhysicalAttemptSlot, PhysicalAttemptTransition, RunDispatch,
    classify_commit_epoch_admission,
};

mod claim;
use claim::{
    claim_exact_transaction, claim_exact_transaction_with_mode, claim_retry_exhausted_transaction,
};

/// Errors from constructing or migrating the dispatch store. Claim/settle-time
/// failures use the neutral [`DispatchError`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed dispatch store.
/// The component namespace for this runtime's tables (see the Postgres store).
/// Built in, not configured — one runtime is one component.
const NS: &str = "runtime";

async fn published_checksum(
    pool: &EmbeddedPool,
    bundle: &str,
    version: i64,
) -> Result<Option<String>, StoreError> {
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
    )
    .bind(format!("{NS}_schema_migrations"))
    .fetch_one(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))?;
    if !ledger_exists {
        return Ok(None);
    }
    sqlx::query_scalar(&format!(
        "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = ?2"
    ))
    .bind(bundle)
    .bind(version)
    .fetch_optional(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))
}

async fn exact_run_replay(
    tx: &mut SqliteConnection,
    prefix: &str,
    request: &RunDispatch,
) -> Result<bool, DispatchError> {
    let run_id = &request.run_id().0;
    let live = sqlx::query_scalar::<_, String>(&format!(
        "SELECT request FROM {prefix}_dispatch WHERE run_id = ?1"
    ))
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_error)?
    .map(|stored| serde_json::from_str::<RunDispatch>(&stored).map_err(json_err))
    .transpose()?;
    let completed = if live.is_none() {
        sqlx::query_scalar::<_, Option<String>>(&format!(
            "SELECT request_fingerprint FROM {prefix}_dispatch_completion WHERE run_id = ?1"
        ))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
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

/// The one SQLite insertion kernel after Run identity and any command-specific
/// admission have been decided in the caller's transaction.
async fn insert_new_dispatch(
    tx: &mut SqliteConnection,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
) -> Result<(), DispatchError> {
    insert_dispatch_with_state(tx, prefix, request, options, "pending", None).await
}

/// The single SQLite row insertion owner. Session Run reservation changes only
/// the initial phase/deadline; canonical identity, dedupe, and supersession stay
/// shared with ordinary enqueue.
async fn insert_dispatch_with_state(
    tx: &mut SqliteConnection,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
    initial_status: &'static str,
    lease_until: Option<i64>,
) -> Result<(), DispatchError> {
    let mut epoch = 0i64;
    if options.supersede {
        epoch = crate::next_supersession_epoch(
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COALESCE(MAX(epoch), 0) FROM {prefix}_dispatch WHERE thread_id = ?1"
            ))
            .bind(&request.thread_id().0)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?,
        )?;
        let candidates = sqlx::query_as::<_, (String, String, i64, i64)>(&format!(
            "SELECT run_id, status, lease_epoch, cancel_requested \
             FROM {prefix}_dispatch WHERE thread_id = ?1"
        ))
        .bind(&request.thread_id().0)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        validate_session_run_replacement_candidates(
            request,
            candidates
                .iter()
                .map(|(_, status, _, _)| {
                    DispatchState::from_db(status).ok_or_else(|| {
                        DispatchError::Rejected(format!(
                            "unknown persisted dispatch state `{status}`"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        for (run_id, status, lease_epoch, cancellation_requested) in candidates {
            let Some(next) = crate::persisted_dispatch_transition(
                &status,
                lease_epoch,
                cancellation_requested != 0,
            )?
            .supersede() else {
                continue;
            };
            sqlx::query(&format!(
                "UPDATE {prefix}_dispatch SET status = ?2, lease_owner = NULL, \
                 lease_until = NULL WHERE run_id = ?1"
            ))
            .bind(run_id)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
    }

    sqlx::query(&format!(
        "INSERT INTO {prefix}_dispatch \
         (run_id, thread_id, request, status, priority, epoch, dedupe_key, lease_until) \
         SELECT ?1,?2,?3,?7,?4,?5,?6,?8 \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM {prefix}_dispatch \
             WHERE dedupe_key = ?6 AND status <> 'dead_letter') \
         AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch_completion WHERE run_id = ?1) \
         ON CONFLICT(run_id) DO NOTHING"
    ))
    .bind(&request.run_id().0)
    .bind(&request.thread_id().0)
    .bind(json(request)?)
    .bind(options.priority)
    .bind(epoch)
    .bind(options.dedupe_key.as_deref())
    .bind(initial_status)
    .bind(lease_until)
    .execute(&mut *tx)
    .await
    .map_err(store_error)?;
    Ok(())
}

async fn known_session_child_threads(
    tx: &mut SqliteConnection,
    prefix: &str,
    parent: &ThreadId,
) -> Result<Vec<ThreadId>, DispatchError> {
    let live_requests =
        sqlx::query_scalar::<_, String>(&format!("SELECT request FROM {prefix}_dispatch"))
            .fetch_all(&mut *tx)
            .await
            .map_err(store_error)?;
    let mut known = Vec::new();
    for stored in live_requests {
        let stored = serde_json::from_str::<RunDispatch>(&stored).map_err(json_err)?;
        if let Some(thread) = session_child_thread(&stored, parent) {
            known.push(thread.clone());
        }
    }
    let rows = sqlx::query_scalar::<_, String>(&format!(
        "SELECT thread_id FROM {prefix}_dispatch_completion \
         WHERE session_thread_id = ?1 AND thread_id IS NOT NULL \
         AND thread_id <> session_thread_id"
    ))
    .bind(&parent.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_error)?;
    for row in rows {
        known.push(ThreadId(row));
    }
    Ok(known)
}

pub struct SqliteDispatchStore {
    pool: EmbeddedPool,
    /// Serializes every dispatch operation with a fenced commit. SQLite is a
    /// single-process backend; an owned guard can therefore span the separate
    /// commit database write without holding a database transaction open.
    authority: Arc<tokio::sync::Mutex<()>>,
    clock: Arc<dyn awaken_time::WallClock>,
}

/// The authority guard and the store's one connection, held together for one
/// dispatch operation. The connection is released before the guard.
type HeldConnection<'a> = (
    tokio::sync::MutexGuard<'a, ()>,
    awaken_sqlite_pool::EmbeddedConnection,
);

impl SqliteDispatchStore {
    /// Open (or create) a database file and apply the dispatch migrations.
    ///
    /// The clock is required for the reason given on
    /// [`MemoryDispatchStore::new`](crate::MemoryDispatchStore::new): this store
    /// judges its own rows, so a default would let a caller believe it had
    /// supplied one when it had not.
    pub async fn open(
        path: &str,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::file(path)
            .open()
            .await
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_pool(pool, clock).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory(
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::memory()
            .open()
            .await
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_pool(pool, clock).await
    }

    async fn from_pool(
        pool: EmbeddedPool,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, StoreError> {
        let published =
            selected_dispatch_bundle(published_checksum(&pool, BUNDLE_ID, 15).await?.as_deref())
                .map_err(|err| StoreError::Migrate(err.to_string()))?;
        let converged = selected_converged_dispatch_bundle(
            published_checksum(&pool, CONVERGED_BUNDLE_ID, 7)
                .await?
                .as_deref(),
        )
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_sqlite_pool::held(runner.run_bundle(&published))
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_sqlite_pool::held(runner.run_bundle(&converged))
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            pool,
            authority: Arc::new(tokio::sync::Mutex::new(())),
            clock,
        })
    }

    /// Take the dispatch authority, then the store's one connection, for a whole
    /// operation. The runtime table namespace is [`NS`].
    async fn with_conn(&self) -> Result<HeldConnection<'_>, DispatchError> {
        let authority = self.authority.lock().await;
        let conn = self.with_conn_unlocked().await?;
        Ok((authority, conn))
    }

    /// Acquire the connection while the caller already owns `authority`.
    async fn with_conn_unlocked(
        &self,
    ) -> Result<awaken_sqlite_pool::EmbeddedConnection, DispatchError> {
        self.pool
            .acquire()
            .await
            .map_err(|err| DispatchError::Rejected(err.to_string()))
    }

    /// Run ids in a terminal-ish dispatch status (dead_letter, superseded), in
    /// enqueue order — backs the operational `dead_letters`/`superseded` queries.
    async fn run_ids_by_status(&self, status: &'static str) -> Result<Vec<RunId>, DispatchError> {
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let ids = sqlx::query_scalar::<_, String>(&format!(
            "SELECT run_id FROM {p}_dispatch WHERE status = ?1 ORDER BY created_at"
        ))
        .bind(status)
        .fetch_all(&mut *conn)
        .await
        .map_err(store_error)?;
        Ok(ids.into_iter().map(RunId).collect())
    }

    async fn lock_claim_epoch_in_status(
        &self,
        claim: &RunClaim,
        required_status: &'static str,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let run = claim.run_id.0.clone();
        let current: Option<ClaimEpochStorageRow<String>> = {
            let mut conn = self.with_conn_unlocked().await?;
            let p = NS;
            sqlx::query_as(&format!(
                "SELECT lease_epoch, lease_owner, lease_until, request, cancel_requested, active_attempt_owner, active_attempt_epoch \
                 FROM {p}_dispatch WHERE run_id = ?1 AND status = ?2"
            ))
            .bind(run)
            .bind(required_status)
            .fetch_optional(&mut *conn)
            .await
            .map_err(store_error)?
        };
        let request = match current {
            None => None,
            Some((
                epoch,
                owner,
                expires_ms,
                request,
                cancellation_requested,
                attempt_owner,
                attempt_epoch,
            )) => {
                let epoch = durable_u64("dispatch lease epoch", epoch)?;
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
                            serde_json::from_str(&request).map_err(json_err)?,
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
        Ok(request.map(
            |(request, expires_ms, cancellation_requested, physical_attempt)| {
                CommitEpochGuard::new(
                    guard,
                    request,
                    expires_ms,
                    cancellation_requested,
                    physical_attempt,
                )
            },
        ))
    }
}

include!("sqlite/physical_attempt.rs");
include!("sqlite/release_claim.rs");
include!("sqlite/dispatch_queue.rs");
include!("sqlite/control_dispatch.rs");
include!("sqlite/summaries.rs");
include!("sqlite/message_ports.rs");
include!("sqlite/pending_rows.rs");

fn store_error(err: sqlx::Error) -> DispatchError {
    DispatchError::unavailable(err)
}

/// Open the `BEGIN IMMEDIATE` transaction every SQLite dispatch mutation runs
/// in: it takes the database write lock before the first read.
async fn immediate(
    conn: &mut SqliteConnection,
) -> Result<sqlx::Transaction<'_, Sqlite>, DispatchError> {
    sqlx::Connection::begin_with(conn, "BEGIN IMMEDIATE")
        .await
        .map_err(store_error)
}

/// A statement's affected-row count in the `usize` the dispatch ports report.
fn affected(done: sqlx::sqlite::SqliteQueryResult) -> usize {
    usize::try_from(done.rows_affected()).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod migration_history_tests {
    use awaken_scoped_migration::MigrationBundle;
    use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
    use awaken_sqlite_pool::EmbeddedSqlite;

    use super::{NS, SqliteDispatchStore};
    use crate::dispatch_schema::{BUNDLE_ID, CONVERGED_BUNDLE_ID, expanded_dispatch_bundle};

    #[tokio::test]
    async fn published_v7_lineages_preserve_receipts_and_converge_once() {
        use crate::dispatch_schema::{
            converged_dispatch_bundle, dispatch_bundle, selected_converged_dispatch_bundle,
        };
        use awaken_scoped_migration::{Dialect, Migration};
        // Causes: C1 V7 is main's entry closure or the deployed operational
        // index; C2 restart after applying V8. Effects: E1 retain exact V7
        // checksum; E2 both effects exist once; E3 reopen creates no receipts.
        // R1=C1 closure -> append index@8; R2=C1 index -> append closure@8;
        // R3=R1/R2+C2 -> same 8 receipts. Unknown V7 is tested separately.
        for index_first in [false, true] {
            let pool = EmbeddedSqlite::memory().open().await.unwrap();
            let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
            runner
                .run_bundle(&dispatch_bundle().unwrap())
                .await
                .unwrap();
            let canonical = converged_dispatch_bundle().unwrap();
            let index = Migration::new(
                7,
                "index exact Run operational replay over the existing global event identities",
                include_str!("migrations/V0008__dispatch_operation_run_sequence.sql").trim(),
            )
            .unwrap();
            let original = if index_first {
                index
            } else {
                canonical.migrations()[6].clone()
            };
            let checksum = original.checksum_for(Dialect::Sqlite);
            let mut prefix = canonical.migrations()[..6].to_vec();
            prefix.push(original);
            runner
                .run_bundle(&MigrationBundle::new(CONVERGED_BUNDLE_ID, prefix).unwrap())
                .await
                .unwrap();
            let store = SqliteDispatchStore::from_pool(pool, awaken_time::system_clock())
                .await
                .unwrap();
            let pool = store.pool.clone();
            let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
            let selected = selected_converged_dispatch_bundle(Some(&checksum)).unwrap();
            runner.run_bundle(&selected).await.unwrap();
            let retained: String = sqlx::query_scalar(
                "SELECT checksum FROM runtime_schema_migrations WHERE bundle_id=?1 AND version=7",
            )
            .bind(CONVERGED_BUNDLE_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(retained, checksum, "E1 immutable V7 receipt");
            let receipts: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM runtime_schema_migrations WHERE bundle_id=?1",
            )
            .bind(CONVERGED_BUNDLE_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(receipts, 8, "E3 exact replay");
            let closed_columns: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pragma_table_info('runtime_dispatch') WHERE name='active_attempt_closing'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            let indexes: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='runtime_dispatch_operation_run_sequence'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!((closed_columns, indexes), (1, 1), "E2 both effects once");
        }
    }

    #[tokio::test]
    async fn unknown_v7_receipt_cannot_select_a_compatibility_stream() {
        use crate::dispatch_schema::{converged_dispatch_bundle, dispatch_bundle};
        // R4 unknown V7 checksum -> ordinary migration mismatch, no repair or
        // receipt overwrite. A lookalike schema never authorizes migration.
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
        runner
            .run_bundle(&dispatch_bundle().unwrap())
            .await
            .unwrap();
        let canonical = converged_dispatch_bundle().unwrap();
        runner
            .run_bundle(
                &MigrationBundle::new(CONVERGED_BUNDLE_ID, canonical.migrations()[..7].to_vec())
                    .unwrap(),
            )
            .await
            .unwrap();
        sqlx::query(
            "UPDATE runtime_schema_migrations SET checksum=?1 WHERE bundle_id=?2 AND version=7",
        )
        .bind("unknown")
        .bind(CONVERGED_BUNDLE_ID)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            matches!(
                SqliteDispatchStore::from_pool(pool, awaken_time::system_clock()).await,
                Err(super::StoreError::Migrate(_))
            ),
            "R4 reject"
        );
    }

    #[tokio::test]
    async fn store_open_selects_and_converges_the_durable_expanded_history() {
        /* Adapter-selection cause/effect table:
         * Q1 no ledger -> fresh compact history; Q2 exact expanded V15 receipt
         * with V1..V24 -> finish expanded V25..V29 then apply converged V1..V7;
         * Q3 reopen Q2 -> no duplicate columns/triggers or receipts.
         * This test owns Q2/Q3; ordinary open-in-memory tests own Q1.
         */
        let directory = tempfile::tempdir().expect("migration database directory");
        let path = directory.path().join("dispatch.db");
        let seed = EmbeddedSqlite::file(&path)
            .open()
            .await
            .expect("seed database");
        let expanded = expanded_dispatch_bundle().expect("expanded history");
        let published = MigrationBundle::new(BUNDLE_ID, expanded.migrations()[..24].to_vec())
            .expect("expanded prefix");
        SqliteMigrationRunner::with_prefix(seed.sqlx().clone(), NS)
            .expect("runner")
            .run_bundle(&published)
            .await
            .expect("seed expanded history");
        let migrated = SqliteDispatchStore::from_pool(seed, awaken_time::system_clock())
            .await
            .expect("Q2 selected migration");
        drop(migrated);
        let reopened = SqliteDispatchStore::open(
            path.to_str().expect("UTF-8 migration path"),
            awaken_time::system_clock(),
        )
        .await
        .expect("Q3 idempotent reopen");
        let published_max: i64 = sqlx::query_scalar(
            "SELECT MAX(version) FROM runtime_schema_migrations WHERE bundle_id = ?1",
        )
        .bind(BUNDLE_ID)
        .fetch_one(&reopened.pool)
        .await
        .expect("Q2 published terminal");
        let converged_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runtime_schema_migrations WHERE bundle_id = ?1",
        )
        .bind(CONVERGED_BUNDLE_ID)
        .fetch_one(&reopened.pool)
        .await
        .expect("Q2 convergence receipt");
        assert_eq!(published_max, 29, "Q2 expanded terminal");
        assert_eq!(
            converged_count, 8,
            "Q2/Q3 seal, paired slot, Thread exclusion, retired epoch, execution binding, retry deadline, entry closure"
        );
    }
}
