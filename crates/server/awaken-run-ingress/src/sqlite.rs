//! SQLite durable implementation of the dispatch-store ports.
//!
//! The embedded sibling of [`PostgresDispatchStore`](crate::PostgresDispatchStore),
//! over the *same* dispatch schema. SQLite has no `FOR UPDATE SKIP LOCKED`, but it
//! does not need it: every claim runs in a `BEGIN IMMEDIATE` transaction that
//! takes the database write lock, so claims serialize and a run is owned by one
//! worker at a time. The claim policy — recover an expired lease, then wake a
//! awaiting run with pending input, then a fresh run — matches
//! the test-support `MemoryDispatchStore` reference backend exactly. The synchronous
//! `rusqlite` driver runs each operation on a blocking thread.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, ClaimEpochStorageRow, Claimed, CommitEpochGuard,
    ContinuationAdmission, CredentialRealizationReceipt, DispatchCompletion, DispatchError,
    DispatchOutcome, DispatchQueue, DispatchState, DispatchSummary, ExactClaimMode, Inbox, Lease,
    Outbox, PendingInput, PendingRecord, RunClaim, RunIdentityDecision, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, StoredRunIdentity, SubmitOptions, can_admit_attempt_credentials,
    classify_completed_session_run_reservation, classify_exact_claim_mode,
    classify_live_session_run_reservation, classify_session_run_reservation_activation,
    compile_attempt_credential_bindings, decide_run_identity, ensure_session_child_capacity,
    installed_worker_credential_capabilities, normalize_pending_millis,
    retry_exhaustion_evidence_is_eligible, session_child_parent, session_child_thread,
    validate_outbox_continuation, validate_session_resume_activity_transition,
    validate_session_resume_evidence, validate_session_resume_target,
    validate_session_run_reservation_request, validate_session_run_reservation_resolution,
    verify_credential_realization_receipt,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, DispatchPlacement, LeaseLossReason, PlacementPolicy, WorkerAssignment,
    WorkerSnapshot, can_assign, can_claim_locally, durable_i64, durable_u64, next_claim_epoch,
    policy_selects_requester,
};
use awaken_run_ingress_contract::RunDispatch;

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

/// Shared SQLite claim predicate for a candidate `d`. Ordinary wake/fresh work
/// waits behind every other Running or Awaiting Run on its Thread. Cancellation
/// waits only behind a Running peer, so historical multi-Awaiting rows can be
/// closed one at a time. Both branches exclude `d` itself.
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

fn exact_run_replay(
    tx: &Transaction<'_>,
    prefix: &str,
    request: &RunDispatch,
) -> Result<bool, DispatchError> {
    let run_id = &request.run_id().0;
    let live = tx
        .query_row(
            &format!("SELECT request FROM {prefix}_dispatch WHERE run_id = ?1"),
            params![run_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(reject)?
        .map(|stored| serde_json::from_str::<RunDispatch>(&stored).map_err(json_err))
        .transpose()?;
    let completed = if live.is_none() {
        tx.query_row(
            &format!(
                "SELECT request_fingerprint FROM {prefix}_dispatch_completion WHERE run_id = ?1"
            ),
            params![run_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(reject)?
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
fn insert_new_dispatch(
    tx: &Transaction<'_>,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
) -> Result<(), DispatchError> {
    insert_dispatch_with_state(tx, prefix, request, options, "pending", None)
}

/// The single SQLite row insertion owner. Session Run reservation changes only
/// the initial phase/deadline; canonical identity, dedupe, and supersession stay
/// shared with ordinary enqueue.
fn insert_dispatch_with_state(
    tx: &Transaction<'_>,
    prefix: &str,
    request: &RunDispatch,
    options: &SubmitOptions,
    initial_status: &'static str,
    lease_until: Option<i64>,
) -> Result<(), DispatchError> {
    let mut epoch = 0i64;
    if options.supersede {
        epoch = crate::next_supersession_epoch(
            tx.query_row(
                &format!(
                    "SELECT COALESCE(MAX(epoch), 0) FROM {prefix}_dispatch WHERE thread_id = ?1"
                ),
                params![request.thread_id().0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(reject)?,
        )?;
        tx.execute(
            &format!(
                "UPDATE {prefix}_dispatch SET status = 'superseded', lease_owner = NULL, \
                 lease_until = NULL WHERE thread_id = ?1 AND status IN ('pending', 'awaiting') \
                 AND cancel_requested = 0"
            ),
            params![request.thread_id().0],
        )
        .map_err(reject)?;
    }

    tx.execute(
        &format!(
            "INSERT INTO {prefix}_dispatch \
             (run_id, thread_id, request, status, priority, epoch, dedupe_key, lease_until) \
             SELECT ?1,?2,?3,?7,?4,?5,?6,?8 \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM {prefix}_dispatch \
                 WHERE dedupe_key = ?6 AND status <> 'dead_letter') \
             AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch_completion WHERE run_id = ?1) \
             ON CONFLICT(run_id) DO NOTHING"
        ),
        params![
            request.run_id().0,
            request.thread_id().0,
            json(request)?,
            options.priority,
            epoch,
            options.dedupe_key,
            initial_status,
            lease_until,
        ],
    )
    .map_err(reject)?;
    Ok(())
}

fn known_session_child_threads(
    tx: &Transaction<'_>,
    prefix: &str,
    parent: &ThreadId,
) -> Result<Vec<ThreadId>, DispatchError> {
    let live_requests = {
        let mut statement = tx
            .prepare(&format!("SELECT request FROM {prefix}_dispatch"))
            .map_err(reject)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(reject)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(reject)?
    };
    let mut known = Vec::new();
    for stored in live_requests {
        let stored = serde_json::from_str::<RunDispatch>(&stored).map_err(json_err)?;
        if let Some(thread) = session_child_thread(&stored, parent) {
            known.push(thread.clone());
        }
    }
    let mut statement = tx
        .prepare(&format!(
            "SELECT thread_id FROM {prefix}_dispatch_completion \
             WHERE session_thread_id = ?1 AND thread_id IS NOT NULL \
             AND thread_id <> session_thread_id"
        ))
        .map_err(reject)?;
    let rows = statement
        .query_map(params![parent.0], |row| row.get::<_, String>(0))
        .map_err(reject)?;
    for row in rows {
        known.push(ThreadId(row.map_err(reject)?));
    }
    Ok(known)
}

pub struct SqliteDispatchStore {
    conn: Arc<Mutex<Connection>>,
    /// Serializes every dispatch operation with a fenced commit. SQLite is a
    /// single-process backend; an owned guard can therefore span the separate
    /// commit database write without exposing a non-Send rusqlite transaction.
    authority: Arc<tokio::sync::Mutex<()>>,
}

impl SqliteDispatchStore {
    /// Open (or create) a database file and apply the dispatch migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            authority: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Run a closure with the locked connection on a blocking thread. The closure
    /// receives the runtime table namespace.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, DispatchError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, DispatchError> + Send + 'static,
    {
        let _authority = self.authority.lock().await;
        self.with_conn_unlocked(f).await
    }

    /// Execute while the caller already owns `authority`.
    async fn with_conn_unlocked<T, F>(&self, f: F) -> Result<T, DispatchError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, DispatchError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| DispatchError::Rejected("dispatch connection poisoned".to_string()))?;
            f(&mut guard, NS)
        })
        .await
        .map_err(|err| DispatchError::Rejected(err.to_string()))?
    }

    /// Run ids in a terminal-ish dispatch status (dead_letter, superseded), in
    /// enqueue order — backs the operational `dead_letters`/`superseded` queries.
    async fn run_ids_by_status(&self, status: &'static str) -> Result<Vec<RunId>, DispatchError> {
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT run_id FROM {p}_dispatch WHERE status = ?1 ORDER BY created_at"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map(params![status], |r| r.get::<_, String>(0))
                .map_err(reject)?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(RunId(row.map_err(reject)?));
            }
            Ok(ids)
        })
        .await
    }

    async fn lock_claim_epoch_in_status(
        &self,
        claim: &RunClaim,
        required_status: &'static str,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let run = claim.run_id.0.clone();
        let current: Option<ClaimEpochStorageRow<String>> = self
            .with_conn_unlocked(move |conn, p| {
                conn.query_row(
                    &format!(
                        "SELECT lease_epoch, lease_owner, lease_until, request, cancel_requested \
                         FROM {p}_dispatch WHERE run_id = ?1 AND status = ?2"
                    ),
                    params![run, required_status],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(reject)
            })
            .await?;
        let request = current
            .map(
                |(epoch, owner, expires_ms, request, cancellation_requested)| {
                    let epoch = durable_u64("dispatch lease epoch", epoch)?;
                    if epoch != claim.epoch || owner.as_deref() != Some(&claim.owner) {
                        return Ok(None);
                    }
                    let expires_ms = expires_ms.ok_or_else(|| {
                        DispatchError::Rejected(
                            "current dispatch claim has no lease expiry".to_string(),
                        )
                    })?;
                    let request = serde_json::from_str(&request).map_err(json_err)?;
                    Ok(Some((
                        request,
                        durable_u64("dispatch lease expiry", expires_ms)?,
                        cancellation_requested != 0,
                    )))
                },
            )
            .transpose()?
            .flatten();
        Ok(
            request.map(|(request, expires_ms, cancellation_requested)| {
                CommitEpochGuard::new(guard, request, expires_ms, cancellation_requested)
            }),
        )
    }
}

include!("sqlite/dispatch_queue.rs");
include!("sqlite/message_ports.rs");
include!("sqlite/pending_rows.rs");
fn reject(err: rusqlite::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
