//! SQLite durable implementation of the dispatch-store ports.
//!
//! The embedded sibling of [`PostgresDispatchStore`](crate::PostgresDispatchStore),
//! over the *same* dispatch schema. SQLite has no `FOR UPDATE SKIP LOCKED`, but it
//! does not need it: every claim runs in a `BEGIN IMMEDIATE` transaction that
//! takes the database write lock, so claims serialize and a run is owned by one
//! worker at a time. The claim policy — recover an expired lease, then wake a
//! awaiting run with pending input, then a fresh run — matches
//! [`MemoryDispatchStore`](crate::MemoryDispatchStore) exactly. The synchronous
//! `rusqlite` driver runs each operation on a blocking thread.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, Claimed, CommitEpochGuard, CredentialRealizationReceipt,
    DispatchCompletion, DispatchError, DispatchOutcome, DispatchQueue, DispatchState,
    DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim, SettleOutcome,
    SubmitOptions, can_admit_attempt_credentials, compile_attempt_credential_bindings,
    installed_worker_credential_capabilities, normalize_pending_millis,
    verify_credential_realization_receipt,
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
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed dispatch store.
/// The component namespace for this runtime's tables (see the Postgres store).
/// Built in, not configured — one runtime is one component.
const NS: &str = "runtime";

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
}

#[async_trait]
impl DispatchQueue for SqliteDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let run = run_id.0.clone();
        let owner = identity.lease_owner();
        self.with_conn(move |conn, p| {
            conn.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM {p}_dispatch \
                     WHERE run_id = ?1 AND status = 'running' \
                     AND lease_owner = ?2 AND lease_until IS NOT NULL \
                     AND lease_until >= ?3)"
                ),
                params![run, owner, crate::clock::db_millis(now_ms)],
                |row| row.get(0),
            )
            .map_err(reject)
        })
        .await
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let run_id = request.run_id().0.clone();
        let thread_id = request.thread_id().0.clone();
        let request_json = json(&request)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // A replayed completed run is a no-op before supersession, so it
            // cannot mutate sibling rows on its thread (ADR-0060).
            let already_known = tx
                .query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM {p}_dispatch WHERE run_id = ?1) OR \
                         EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1)"
                    ),
                    params![run_id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(reject)?;
            if already_known {
                tx.commit().map_err(reject)?;
                return Ok(());
            }

            // Supersession: take the highest epoch on the thread and mark its
            // prior pending/awaiting work superseded — newest wins (ADR-0022).
            let mut epoch = 0i64;
            if options.supersede {
                epoch = tx
                    .query_row(
                        &format!(
                            "SELECT COALESCE(MAX(epoch), 0) FROM {p}_dispatch WHERE thread_id = ?1"
                        ),
                        params![thread_id],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(reject)?
                    + 1;
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET status = 'superseded', lease_owner = NULL, \
                         lease_until = NULL WHERE thread_id = ?1 AND status IN ('pending', 'awaiting') \
                         AND cancel_requested = 0"
                    ),
                    params![thread_id],
                )
                .map_err(reject)?;
            }

            // Insert unless the run id exists or a live dispatch already carries
            // the dedupe key. A NULL dedupe key never matches.
            tx.execute(
                &format!(
                    "INSERT INTO {p}_dispatch \
                     (run_id, thread_id, request, status, priority, epoch, dedupe_key) \
                     SELECT ?1,?2,?3,'pending',?4,?5,?6 \
                     WHERE NOT EXISTS ( \
                         SELECT 1 FROM {p}_dispatch \
                         WHERE dedupe_key = ?6 AND status <> 'dead_letter') \
                     AND NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1) \
                     ON CONFLICT(run_id) DO NOTHING"
                ),
                params![
                    run_id,
                    thread_id,
                    request_json,
                    options.priority,
                    epoch,
                    options.dedupe_key
                ],
            )
            .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(())
        })
        .await
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
        let run_id = request.run_id().0.clone();
        let thread_id = request.thread_id().0.clone();
        let request_json = json(&request)?;
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let inserted = tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
                         SELECT ?1,?2,?3,'pending' \
                         WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![run_id, thread_id, request_json],
                )
                .map_err(reject)?;
            let _ = inserted;
            let claimed =
                claim_exact_transaction(
                    &tx,
                    &run_id,
                    &owner,
                    lease_ms,
                    now_ms,
                    None,
                    &capabilities,
                )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
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
        let run_id = request.run_id().0.clone();
        let thread_id = request.thread_id().0.clone();
        let request_json = json(&request)?;
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let owner = worker.identity.lease_owner();
            tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
                         SELECT ?1,?2,?3,'pending' \
                         WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![run_id, thread_id, request_json],
                )
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
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
        let run_id = input.run_id.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            append_pending_row(&tx, p, &input)?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            append_pending_row(&tx, p, &input)?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let run = claim.run_id.0.clone();
        let current: Option<(u64, Option<String>, Option<i64>, String)> = self
            .with_conn_unlocked(move |conn, p| {
                conn.query_row(
                    &format!(
                        "SELECT lease_epoch, lease_owner, lease_until, request \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![run],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?.max(0) as u64,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(reject)
            })
            .await?;
        let request = current
            .filter(|(epoch, owner, _, _)| {
                *epoch == claim.epoch && owner.as_deref() == Some(&claim.owner)
            })
            .map(|(_, _, expires_ms, request)| {
                let expires_ms = expires_ms.ok_or_else(|| {
                    DispatchError::Rejected(
                        "current dispatch claim has no lease expiry".to_string(),
                    )
                })?;
                let request = serde_json::from_str(&request).map_err(json_err)?;
                Ok((request, expires_ms.max(0) as u64))
            })
            .transpose()?;
        Ok(request.map(|(request, expires_ms)| CommitEpochGuard::new(guard, request, expires_ms)))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // Priority: recover an expired lease, then wake an awaiting run with
            // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
            // the IMMEDIATE transaction is the single-owner guard.
            let local_eligible = "(d.cancel_requested = 1 OR (\
                COALESCE(json_extract(d.request, '$.placement.location'), 'remote_preferred') \
                    <> 'remote_required' AND \
                COALESCE(json_array_length(json_extract(\
                    d.request, '$.placement.required_credentials')), 0) = 0))";
            let recovery = format!(
                "SELECT d.run_id FROM {p}_dispatch d \
                 WHERE d.status = 'running' AND d.lease_until IS NOT NULL \
                 AND d.lease_until < ?1 AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.created_at LIMIT 1"
            );
            // Single-writer-per-thread (ADR-0022): a wake or fresh pick skips any
            // thread that already has a run in flight. Recovery (above) is exempt —
            // it re-owns the SAME running row rather than adding a second.
            let not_running = format!(
                "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
                 WHERE r.thread_id = d.thread_id AND r.status = 'running')"
            );
            let wake = format!(
                "SELECT run_id FROM {p}_dispatch d \
                 WHERE d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
                     SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                     AND (pe.available_at IS NULL OR pe.available_at <= ?1))) \
                 AND {not_running} AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.created_at LIMIT 1"
            );
            let fresh = format!(
                "SELECT run_id FROM {p}_dispatch d \
                 WHERE d.status = 'pending' AND {not_running} AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.priority DESC, d.created_at LIMIT 1"
            );

            let row = |sql: &str, bind_now: bool| -> Result<Option<String>, DispatchError> {
                let map = |r: &rusqlite::Row| r.get::<_, String>(0);
                if bind_now {
                    tx.query_row(sql, params![crate::clock::db_millis(now_ms)], map)
                } else {
                    tx.query_row(sql, [], map)
                }
                .optional()
                .map_err(reject)
            };

            let picked = match row(&recovery, true)? {
                Some(found) => Some(found),
                None => match row(&wake, true)? {
                    Some(found) => Some(found),
                    None => row(&fresh, false)?,
                },
            };

            let Some(run_id) = picked else {
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let sql = format!(
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   ) AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
                 (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
                   WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
                 ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                          d.priority DESC, d.created_at"
            );
            let capabilities = installed_worker_credential_capabilities(&worker)?;
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(reject)?;
                let rows = stmt
                    .query_map(params![crate::clock::db_millis(now_ms)], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, i64>(4)? != 0,
                            row.get::<_, i64>(5)?.max(0) as u64,
                        ))
                    })
                    .map_err(reject)?;
                let mut selected = None;
                for row in rows {
                    let (
                        run_id,
                        request_json,
                        sandbox,
                        previous_json,
                        cancellation_requested,
                        lease_epoch,
                    ) = row.map_err(reject)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    if cancellation_requested
                        || (can_assign(
                            &worker,
                            &request.placement,
                            previous.as_ref(),
                            sandbox.is_some(),
                            now_ms,
                        )
                        .is_ok()
                            && lease_epoch.checked_add(1).is_some_and(|epoch| {
                                can_admit_attempt_credentials(
                                    &request,
                                    &capabilities,
                                    epoch,
                                    now_ms,
                                )
                            }))
                    {
                        selected = Some(run_id);
                        break;
                    }
                }
                selected
            };
            let Some(run_id) = selected else {
                tx.commit().map_err(reject)?;
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requester = requester.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let sql = format!(
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   ) AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
                 (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
                   WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
                 ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                          d.priority DESC, d.created_at"
            );
            let capabilities = installed_worker_credential_capabilities(&requester)?;
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(reject)?;
                let rows = stmt
                    .query_map(params![crate::clock::db_millis(now_ms)], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)? != 0,
                            row.get::<_, i64>(6)?.max(0) as u64,
                        ))
                    })
                    .map_err(reject)?;
                let mut selected = None;
                for row in rows {
                    let (
                        run_id,
                        request_json,
                        sandbox,
                        previous_json,
                        status,
                        cancellation_requested,
                        lease_epoch,
                    ) = row.map_err(reject)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    if cancellation_requested
                        || (policy_selects_requester(
                            &request,
                            policy.as_ref(),
                            DispatchPlacement {
                                recovered: status == "running",
                                previous: previous.as_ref(),
                                sandbox_bound: sandbox.is_some(),
                                requester: &requester.identity,
                                workers: &workers,
                                now_ms,
                            },
                        )? && lease_epoch.checked_add(1).is_some_and(|epoch| {
                            can_admit_attempt_credentials(
                                &request,
                                &capabilities,
                                epoch,
                                now_ms,
                            )
                        }))
                    {
                        selected = Some(run_id);
                        break;
                    }
                }
                selected
            };
            let Some(run_id) = selected else {
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &requester.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&requester),
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET lease_until = ?1 \
                         WHERE run_id = ?2 AND status = 'running' AND lease_owner = ?3"
                    ),
                    params![
                        crate::clock::db_millis(crate::clock::deadline_millis(now_ms, lease_ms)),
                        run_id,
                        owner
                    ],
                )
                .map_err(reject)?;
            Ok(n > 0)
        })
        .await
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        let sandbox_ref = sandbox_ref.to_string();
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET sandbox = ?1 WHERE run_id = ?2 \
                     AND status = 'running' AND lease_owner = ?3 AND lease_epoch = ?4"
                    ),
                    params![sandbox_ref, claim.run_id.0, claim.owner, claim.epoch as i64],
                )
                .map_err(reject)?;
            Ok(if changed == 1 {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            })
        })
        .await
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(Option<String>, Option<String>)> = tx
                .query_row(
                    &format!(
                        "SELECT credential_bindings, credential_receipts FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status = 'running' \
                         AND lease_owner = ?2 AND lease_epoch = ?3"
                    ),
                    params![claim.run_id.0, claim.owner, claim.epoch as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            let Some((bindings_json, receipts_json)) = current else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let bindings: Vec<AttemptCredentialBinding> = bindings_json
                .map(|value| serde_json::from_str(&value).map_err(json_err))
                .transpose()?
                .unwrap_or_default();
            verify_credential_realization_receipt(&bindings, &receipt)
                .map_err(|error| DispatchError::Rejected(error.to_string()))?;
            let mut receipts: Vec<CredentialRealizationReceipt> = receipts_json
                .map(|value| serde_json::from_str(&value).map_err(json_err))
                .transpose()?
                .unwrap_or_default();
            if let Some(existing) = receipts
                .iter()
                .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint)
            {
                if existing != &receipt {
                    return Err(DispatchError::Rejected(
                        "credential realization receipt conflicts with committed evidence"
                            .to_string(),
                    ));
                }
                tx.commit().map_err(reject)?;
                return Ok(SettleOutcome::Applied);
            }
            receipts.push(receipt);
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET credential_receipts = ?1 \
                         WHERE run_id = ?2 AND status = 'running' \
                         AND lease_owner = ?3 AND lease_epoch = ?4"
                    ),
                    params![
                        json(&receipts)?,
                        claim.run_id.0,
                        claim.owner,
                        claim.epoch as i64
                    ],
                )
                .map_err(reject)?;
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        self.with_conn(move |conn, p| {
            let depth: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
                         d.status = 'pending' OR \
                         (d.status = 'running' AND d.lease_until < ?1) OR \
                         (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
                           SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
                           AND (i.available_at IS NULL OR i.available_at <= ?1)\
                         )))"
                    ),
                    params![crate::clock::db_millis(now_ms)],
                    |row| row.get(0),
                )
                .map_err(reject)?;
            Ok(Some(depth as u64))
        })
        .await
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            // Only rows within half a lease of expiring; a fresh claim is a full
            // length out and is skipped until it approaches expiry (ADR-0024).
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET lease_until = ?1 \
                         WHERE status = 'running' AND lease_owner = ?2 \
                         AND lease_until IS NOT NULL AND lease_until < ?3"
                    ),
                    params![
                        crate::clock::db_millis(crate::clock::deadline_millis(now_ms, lease_ms)),
                        owner,
                        crate::clock::db_millis(crate::clock::deadline_millis(
                            now_ms,
                            lease_ms / 2
                        ))
                    ],
                )
                .map_err(reject)?;
            Ok(n)
        })
        .await
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let run_id = run_id.0.clone();
        let consumed = consumed.to_vec();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let owner: Option<String> = tx
                .query_row(
                    &format!(
                        "SELECT lease_owner FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status = 'running' AND lease_epoch = ?2"
                    ),
                    params![run_id, epoch as i64],
                    |row| row.get(0),
                )
                .optional()
                .map_err(reject)?
                .flatten();
            let Some(owner) = owner else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let claim = RunClaim {
                run_id: RunId(run_id.clone()),
                owner,
                epoch,
            };
            // Fence first: mutate the dispatch row ONLY while the caller still holds
            // the current epoch. A stale owner (lower epoch) affects zero rows, so
            // its settle touches neither the dispatch nor its pending.
            let dispatch_rows = match outcome {
                DispatchOutcome::Done => tx
                    .execute(
                        &format!(
                            "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                             AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch as i64],
                    )
                    .map_err(reject)?,
                DispatchOutcome::Awaiting => tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = 'awaiting', lease_owner = NULL, \
                             lease_until = NULL, attempt_count = 0 \
                             WHERE run_id = ?1 AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch as i64],
                    )
                    .map_err(reject)?,
            };
            if dispatch_rows == 0 {
                // Fenced: re-claimed under a higher epoch (or already gone). Change
                // nothing and report the loss so the stale caller abandons.
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            if outcome == DispatchOutcome::Done {
                tx.execute(
                    &format!(
                        "INSERT INTO {p}_dispatch_completion (run_id) VALUES (?1) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![run_id],
                )
                .map_err(reject)?;
            }
            // The fence held; now reconcile the run's pending input.
            match outcome {
                DispatchOutcome::Done => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                        params![run_id],
                    )
                    .map_err(reject)?;
                    // Also drop anything else consumed this attempt (e.g. unbound
                    // idle-thread input, ADR-0021).
                    for message_id in &consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(reject)?;
                    }
                }
                DispatchOutcome::Awaiting => {
                    for message_id in &consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(reject)?;
                    }
                }
            }
            insert_operation(&tx, p, &DispatchOperation::Settled { claim, outcome })?;
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
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
            DispatchError::Rejected("completion cursor exceeds INTEGER range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT sequence, run_id FROM {p}_dispatch_completion \
                     WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
                ))
                .map_err(reject)?;
            let rows = statement
                .query_map(params![after_sequence, limit], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(reject)?;
            let mut events = Vec::new();
            for row in rows {
                let (sequence, run_id) = row.map_err(reject)?;
                events.push(DispatchCompletion {
                    sequence: u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted completion sequence is negative".to_string(),
                        )
                    })?,
                    run_id: RunId(run_id),
                });
            }
            Ok(events)
        })
        .await
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let candidates = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT run_id, lease_owner, lease_epoch, attempt_count \
                         FROM {p}_dispatch WHERE status = 'running' \
                         AND lease_until IS NOT NULL AND lease_until < ?1 \
                         AND attempt_count >= ?2 ORDER BY created_at"
                    ))
                    .map_err(reject)?;
                let rows = statement
                    .query_map(
                        params![crate::clock::db_millis(now_ms), max_attempts as i64],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .map_err(reject)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(reject)?
            };
            let mut reaped = 0;
            for (run_id, owner, epoch, attempt_count) in candidates {
                let Some(owner) = owner else {
                    return Err(DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    ));
                };
                let changed = tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = 'dead_letter', \
                             lease_owner = NULL, lease_until = NULL, dead_lettered_at = ?1 \
                             WHERE run_id = ?2 AND status = 'running' AND lease_epoch = ?3"
                        ),
                        params![crate::clock::db_millis(now_ms), run_id, epoch],
                    )
                    .map_err(reject)?;
                if changed == 0 {
                    continue;
                }
                let claim = RunClaim {
                    run_id: RunId(run_id),
                    owner,
                    epoch: epoch.max(0) as u64,
                };
                insert_operation(
                    &tx,
                    p,
                    &DispatchOperation::LeaseLost {
                        claim: claim.clone(),
                        reason: LeaseLossReason::RetryExhausted,
                    },
                )?;
                insert_operation(
                    &tx,
                    p,
                    &DispatchOperation::DeadLettered {
                        claim,
                        attempt_count: attempt_count.max(0) as u64,
                    },
                )?;
                reaped += 1;
            }
            tx.commit().map_err(reject)?;
            Ok(reaped)
        })
        .await
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("dead_letter").await
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("superseded").await
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT run_id, thread_id, status, attempt_count, cancel_requested, sandbox FROM {p}_dispatch \
                     ORDER BY created_at"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)? != 0,
                        r.get::<_, Option<String>>(5)?.is_some(),
                    ))
                })
                .map_err(reject)?;
            let mut out = Vec::new();
            for row in rows {
                let (run_id, thread_id, status, attempt_count, cancellation_requested, sandbox_bound) =
                    row.map_err(reject)?;
                let state = DispatchState::from_db(&status).ok_or_else(|| {
                    DispatchError::Rejected(format!("unknown persisted dispatch state {status}"))
                })?;
                out.push(DispatchSummary {
                    run_id: RunId(run_id),
                    thread_id: ThreadId(thread_id),
                    state,
                    cancellation_requested,
                    attempt_count: attempt_count as u64,
                    sandbox_bound,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
        self.with_conn(move |conn, p| {
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET status = 'pending', attempt_count = 0, \
                         lease_owner = NULL, lease_until = NULL \
                         WHERE run_id = ?1 AND status = 'dead_letter'"
                    ),
                    params![run_id],
                )
                .map_err(reject)?;
            Ok(n > 0)
        })
        .await
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let run_id = run_id.0.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(String, String, Option<String>, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT thread_id, status, lease_owner, lease_epoch FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status IN ('pending', 'awaiting', 'running')"
                    ),
                    params![run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(reject)?;
            if let Some((_, status, _, _)) = &current {
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET cancel_requested = 1, \
                         lease_epoch = lease_epoch + CASE WHEN status = 'running' THEN 1 ELSE 0 END, \
                         lease_owner = CASE WHEN status = 'running' THEN NULL ELSE lease_owner END, \
                         lease_until = CASE WHEN status = 'running' THEN NULL ELSE lease_until END, \
                         status = CASE WHEN status = 'running' THEN 'pending' ELSE status END \
                         WHERE run_id = ?1"
                    ),
                    params![run_id],
                )
                .map_err(reject)?;
                if status == "running" {
                    let (_, _, owner, epoch) = current.as_ref().expect("current exists");
                    insert_operation(
                        &tx,
                        p,
                        &DispatchOperation::LeaseLost {
                            claim: RunClaim {
                                run_id: RunId(run_id.clone()),
                                owner: owner.clone().ok_or_else(|| {
                                    DispatchError::Rejected(
                                        "running dispatch has no persisted lease owner".to_string(),
                                    )
                                })?,
                                epoch: (*epoch).max(0) as u64,
                            },
                            reason: LeaseLossReason::Cancelled,
                        },
                    )?;
                }
            }
            tx.commit().map_err(reject)?;
            Ok(current.map(|(thread, _, _, _)| ThreadId(thread)))
        })
        .await
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let thread_id = thread_id.0.clone();
        self.with_conn(move |conn, p| {
            let run: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT run_id FROM {p}_dispatch WHERE thread_id = ?1 AND status = 'awaiting' \
                         AND cancel_requested = 0 \
                         ORDER BY created_at LIMIT 1"
                    ),
                    params![thread_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(reject)?;
            Ok(run.map(RunId))
        })
        .await
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE status = 'dead_letter')"
                ),
                [],
            )
            .map_err(reject)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE status = 'dead_letter'"),
                    [],
                )
                .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(n)
        })
        .await
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let cond = "status = 'dead_letter' AND dead_lettered_at IS NOT NULL \
                        AND dead_lettered_at <= ?1";
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE {cond})"
                ),
                params![crate::clock::db_millis(cutoff_ms)],
            )
            .map_err(reject)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE {cond}"),
                    params![crate::clock::db_millis(cutoff_ms)],
                )
                .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(n)
        })
        .await
    }
}

#[async_trait]
impl Inbox for SqliteDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        self.with_conn(move |conn, p| append_pending_row(conn, p, &input))
            .await
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let thread = thread_id.clone();
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT message_id, run_id, correlation_id, result, revision, available_at \
                     FROM {p}_pending WHERE thread_id = ?1 ORDER BY created_at"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map(params![thread.0], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                    ))
                })
                .map_err(reject)?;
            let mut records = Vec::new();
            for row in rows {
                let (message_id, run_id, correlation_id, result, revision, available_at) =
                    row.map_err(reject)?;
                records.push(PendingRecord {
                    input: PendingInput {
                        message_id,
                        run_id: RunId(run_id),
                        thread_id: thread.clone(),
                        correlation_id,
                        available_at_ms: available_at
                            .map(crate::clock::millis_from_db)
                            .transpose()
                            .map_err(|err| DispatchError::Rejected(err.to_string()))?,
                        result: serde_json::from_str(&result).map_err(json_err)?,
                    },
                    revision: revision as u64,
                });
            }
            Ok(records)
        })
        .await
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let outcome = match current_revision(&tx, p, &message_id)? {
                None => CasOutcome::NotFound,
                Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
                Some(_) => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                        params![message_id],
                    )
                    .map_err(reject)?;
                    CasOutcome::Applied
                }
            };
            tx.commit().map_err(reject)?;
            Ok(outcome)
        })
        .await
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        let result_json = json(&result)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let outcome = match current_revision(&tx, p, &message_id)? {
                None => CasOutcome::NotFound,
                Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
                Some(_) => {
                    tx.execute(
                        &format!(
                            "UPDATE {p}_pending SET result = ?1, revision = revision + 1 \
                             WHERE message_id = ?2"
                        ),
                        params![result_json, message_id],
                    )
                    .map_err(reject)?;
                    CasOutcome::Applied
                }
            };
            tx.commit().map_err(reject)?;
            Ok(outcome)
        })
        .await
    }
}

#[async_trait]
impl DispatchOperationalFeed for SqliteDispatchStore {
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
            DispatchError::Rejected("dispatch cursor exceeds INTEGER range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn, prefix| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT sequence, recorded_at_ms, operation FROM {prefix}_dispatch_operation \
                     WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
                ))
                .map_err(reject)?;
            let rows = statement
                .query_map(params![after, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(reject)?;
            let mut events = Vec::new();
            for row in rows {
                let (sequence, recorded_at_ms, operation) = row.map_err(reject)?;
                events.push(DispatchOperationalEvent {
                    cursor: DispatchCursor(u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted dispatch operation sequence is negative".to_string(),
                        )
                    })?),
                    recorded_at_ms: recorded_at_ms.map(u64::try_from).transpose().map_err(
                        |_| {
                            DispatchError::Rejected(
                                "persisted dispatch operation time is negative".to_string(),
                            )
                        },
                    )?,
                    operation: serde_json::from_str(&operation).map_err(json_err)?,
                });
            }
            let next_cursor = events.last().map_or(cursor, |event| event.cursor);
            Ok(DispatchPage {
                events,
                next_cursor,
            })
        })
        .await
    }
}

#[async_trait]
impl Outbox for SqliteDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let input = normalize_pending_millis(input);
        let payload = json(&input)?;
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_outbox (message_id, payload) VALUES (?1,?2) \
                         ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![&input.message_id, payload],
                )
                .map_err(reject)?;
            if changed > 0 {
                return Ok(true);
            }
            let existing = conn
                .query_row(
                    &format!("SELECT payload FROM {p}_outbox WHERE message_id = ?1"),
                    params![&input.message_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(reject)?
                .map(|stored| serde_json::from_str::<PendingInput>(&stored).map_err(json_err))
                .transpose()?;
            match existing {
                Some(existing) if existing == input => Ok(false),
                Some(_) => Err(idempotency_conflict(&input.message_id, "outbox")),
                None => Err(DispatchError::Rejected(format!(
                    "outbox `{}` vanished during idempotency validation",
                    input.message_id
                ))),
            }
        })
        .await
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let staged: Vec<(String, String)> = {
                let mut stmt = conn
                    .prepare(&format!("SELECT message_id, payload FROM {p}_outbox"))
                    .map_err(reject)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(reject)?;
                rows.collect::<Result<_, _>>().map_err(reject)?
            };

            let mut relayed = 0;
            for (message_id, payload) in staged {
                let input = normalize_pending_millis(
                    serde_json::from_str::<PendingInput>(&payload).map_err(json_err)?,
                );
                // One transaction per message: idempotent target append, then
                // drop the outbox row.
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(reject)?;
                append_pending_row(&tx, p, &input)?;
                tx.execute(
                    &format!("DELETE FROM {p}_outbox WHERE message_id = ?1"),
                    params![message_id],
                )
                .map_err(reject)?;
                tx.commit().map_err(reject)?;
                relayed += 1;
            }
            Ok(relayed)
        })
        .await
    }
}

fn current_revision(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    tx.query_row(
        &format!("SELECT revision FROM {prefix}_pending WHERE message_id = ?1"),
        params![message_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(reject)
    .map(|opt| opt.map(|r| r as u64))
}

fn pending_for_run(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    run_id: &str,
    now_ms: u64,
) -> Result<Vec<PendingInput>, DispatchError> {
    let mut stmt = tx
        .prepare(&format!(
            "SELECT message_id, thread_id, correlation_id, result, available_at \
             FROM {prefix}_pending WHERE run_id = ?1 \
             AND (available_at IS NULL OR available_at <= ?2) ORDER BY created_at"
        ))
        .map_err(reject)?;
    let rows = stmt
        .query_map(params![run_id, crate::clock::db_millis(now_ms)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(reject)?;
    let mut pending = Vec::new();
    for row in rows {
        let (message_id, thread_id, correlation_id, result, available_at) = row.map_err(reject)?;
        pending.push(PendingInput {
            message_id,
            run_id: RunId(run_id.to_string()),
            thread_id: ThreadId(thread_id),
            correlation_id,
            available_at_ms: available_at
                .map(crate::clock::millis_from_db)
                .transpose()
                .map_err(|err| DispatchError::Rejected(err.to_string()))?,
            result: serde_json::from_str(&result).map_err(json_err)?,
        });
    }
    Ok(pending)
}

fn insert_operation(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    operation: &DispatchOperation,
) -> Result<(), DispatchError> {
    let recorded_at_ms = i64::try_from(crate::clock::system_now_ms()).unwrap_or(i64::MAX);
    tx.execute(
        &format!(
            "INSERT INTO {prefix}_dispatch_operation \
             (run_id, operation, recorded_at_ms) VALUES (?1, ?2, ?3)"
        ),
        params![operation.run_id().0, json(operation)?, recorded_at_ms],
    )
    .map_err(reject)?;
    Ok(())
}

fn claim_exact_transaction(
    tx: &rusqlite::Transaction<'_>,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let prefix = NS;
    type PickedDispatch = (
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        i64,
    );
    let not_running = format!(
        "NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.status = 'running')"
    );
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch FROM {prefix}_dispatch d \
         WHERE d.run_id = ?1 AND ( \
           (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?2) \
           OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
             SELECT 1 FROM {prefix}_pending pe WHERE pe.run_id = d.run_id \
             AND (pe.available_at IS NULL OR pe.available_at <= ?2))) AND {not_running}) \
           OR (d.status = 'pending' AND {not_running}) \
         ) LIMIT 1"
    );
    let picked: Option<PickedDispatch> = tx
        .query_row(
            &sql,
            params![requested_run, crate::clock::db_millis(now_ms)],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(reject)?;
    let Some((
        request_json,
        sandbox,
        status,
        previous_json,
        cancellation_requested,
        previous_owner,
        previous_epoch,
    )) = picked
    else {
        return Ok(None);
    };
    let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
    let previous: Option<WorkerAssignment> = previous_json
        .map(|value| serde_json::from_str(&value).map_err(json_err))
        .transpose()?;
    if cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref(),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let claim_epoch = (previous_epoch.max(0) as u64)
        .checked_add(1)
        .ok_or_else(|| DispatchError::Rejected("dispatch claim epoch exhausted".to_string()))?;
    let credential_bindings = if cancellation_requested != 0 {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&request, capabilities, claim_epoch, now_ms).map_err(
            |error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            },
        )?
    };
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    tx.execute(
        &format!(
            "UPDATE {prefix}_dispatch SET status = 'running', lease_owner = ?1, \
             lease_until = ?2, attempt_count = attempt_count + ?3, \
             lease_epoch = ?5, worker_assignment = ?6, credential_bindings = ?7, \
             credential_receipts = ?8 WHERE run_id = ?4"
        ),
        params![
            owner,
            crate::clock::db_millis(expires),
            i64::from(status == "running"),
            requested_run,
            claim_epoch as i64,
            worker
                .map(WorkerAssignment::from)
                .map(|value| json(&value))
                .transpose()?,
            json(&credential_bindings)?,
            json(&Vec::<CredentialRealizationReceipt>::new())?
        ],
    )
    .map_err(reject)?;
    let claim = RunClaim {
        run_id: RunId(requested_run.to_string()),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if status == "running" {
        let previous = RunClaim {
            run_id: RunId(requested_run.to_string()),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: previous_epoch.max(0) as u64,
        };
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason: LeaseLossReason::Expired,
            },
        )?;
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
        )?;
    } else {
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
        )?;
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
        pending: pending_for_run(tx, prefix, requested_run, now_ms)?,
        recovered: status == "running",
        sandbox,
        assignment: worker.map(WorkerAssignment::from),
    }))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, DispatchError> {
    serde_json::to_string(value).map_err(json_err)
}

/// The one SQLite pending insert path, reused by direct delivery, Inbox append,
/// and outbox relay so exact retries and identity conflicts cannot diverge.
fn append_pending_row(
    conn: &Connection,
    prefix: &str,
    input: &PendingInput,
) -> Result<bool, DispatchError> {
    let changed = conn
        .execute(
            &format!(
                "INSERT INTO {prefix}_pending \
                 (message_id, run_id, thread_id, correlation_id, result, available_at) \
                 VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(message_id) DO NOTHING"
            ),
            params![
                &input.message_id,
                &input.run_id.0,
                &input.thread_id.0,
                &input.correlation_id,
                json(&input.result)?,
                input.available_at_ms.map(crate::clock::db_millis)
            ],
        )
        .map_err(reject)?;
    if changed > 0 {
        return Ok(true);
    }
    match load_pending_input(conn, prefix, &input.message_id)? {
        Some(existing) if existing == *input => Ok(false),
        Some(_) => Err(idempotency_conflict(&input.message_id, "pending-input")),
        None => Err(DispatchError::Rejected(format!(
            "pending-input `{}` vanished during idempotency validation",
            input.message_id
        ))),
    }
}

fn load_pending_input(
    conn: &Connection,
    prefix: &str,
    message_id: &str,
) -> Result<Option<PendingInput>, DispatchError> {
    conn.query_row(
        &format!(
            "SELECT run_id, thread_id, correlation_id, result, available_at \
             FROM {prefix}_pending WHERE message_id = ?1"
        ),
        params![message_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        },
    )
    .optional()
    .map_err(reject)?
    .map(
        |(run_id, thread_id, correlation_id, result, available_at)| {
            Ok(PendingInput {
                message_id: message_id.to_string(),
                run_id: RunId(run_id),
                thread_id: ThreadId(thread_id),
                correlation_id,
                available_at_ms: available_at.map(u64::try_from).transpose().map_err(|_| {
                    DispatchError::Rejected(format!(
                        "pending-input `{message_id}` has a negative delivery time"
                    ))
                })?,
                result: serde_json::from_str(&result).map_err(json_err)?,
            })
        },
    )
    .transpose()
}

fn idempotency_conflict(message_id: &str, aggregate: &str) -> DispatchError {
    DispatchError::Rejected(format!(
        "idempotency key `{message_id}` was reused with another {aggregate} payload"
    ))
}

fn json_err(err: serde_json::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}

fn reject(err: rusqlite::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
