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
    CasOutcome, Claimed, CommitEpochGuard, DispatchCompletion, DispatchError, DispatchOutcome,
    DispatchQueue, DispatchState, DispatchSummary, Inbox, Lease, Outbox, PendingInput,
    PendingRecord, RunClaim, SettleOutcome, SubmitOptions,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::{WorkerAssignment, WorkerSnapshot, can_assign};
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

    /// Open a private in-memory database.
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
                         lease_until = NULL WHERE thread_id = ?1 AND status IN ('pending', 'awaiting')"
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let run_id = request.run_id().0.clone();
        let thread_id = request.thread_id().0.clone();
        let request_json = json(&request)?;
        let owner = owner.to_string();
        let expires = now_ms + lease_ms;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let inserted = tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_dispatch \
                         (run_id, thread_id, request, status, lease_owner, lease_until, lease_epoch) \
                         SELECT ?1,?2,?3,'running',?4,?5,1 \
                         WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![run_id, thread_id, request_json, owner, expires as i64],
                )
                .map_err(reject)?;
            if inserted == 1 {
                tx.commit().map_err(reject)?;
                return Ok(Some(Claimed {
                    request,
                    lease: Lease {
                        run_id: RunId(run_id),
                        owner,
                        expires_ms: expires,
                        epoch: 1,
                    },
                    pending: Vec::new(),
                    recovered: false,
                    sandbox: None,
                    assignment: None,
                }));
            }

            // A retry follows the same exact-claim kernel as explicit recovery.
            let claimed =
                claim_exact_transaction(&tx, p, &run_id, &owner, lease_ms, now_ms, None)?;
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
        let expires = now_ms + lease_ms;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let owner = worker.identity.lease_owner();
            let assignment = WorkerAssignment::from(&worker);
            let inserted = tx
                .execute(
                    &format!(
                        "INSERT INTO {p}_dispatch \
                         (run_id, thread_id, request, status, lease_owner, lease_until, lease_epoch, worker_assignment) \
                         SELECT ?1,?2,?3,'running',?4,?5,1,?6 \
                         WHERE NOT EXISTS (SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![
                        run_id,
                        thread_id,
                        request_json,
                        owner,
                        expires as i64,
                        json(&assignment)?
                    ],
                )
                .map_err(reject)?;
            if inserted == 1 {
                tx.commit().map_err(reject)?;
                return Ok(Some(Claimed {
                    request,
                    lease: Lease {
                        run_id: RunId(run_id),
                        owner,
                        expires_ms: expires,
                        epoch: 1,
                    },
                    pending: Vec::new(),
                    recovered: false,
                    sandbox: None,
                    assignment: Some(assignment),
                }));
            }
            let claimed = claim_exact_transaction(
                &tx,
                p,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                Some(&worker),
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let run_id = input.run_id.0.clone();
        let owner = owner.to_string();
        let result_json = json(&input.result)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "INSERT INTO {p}_pending \
                     (message_id, run_id, thread_id, correlation_id, result, available_at) \
                     VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(message_id) DO NOTHING"
                ),
                params![
                    input.message_id,
                    input.run_id.0,
                    input.thread_id.0,
                    input.correlation_id,
                    result_json,
                    input.available_at_ms.map(|time| time as i64)
                ],
            )
            .map_err(reject)?;
            let claimed = claim_exact_transaction(&tx, p, &run_id, &owner, lease_ms, now_ms, None)?;
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
        let run_id = input.run_id.0.clone();
        let worker = worker.clone();
        let result_json = json(&input.result)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "INSERT INTO {p}_pending \
                     (message_id, run_id, thread_id, correlation_id, result, available_at) \
                     VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(message_id) DO NOTHING"
                ),
                params![
                    input.message_id,
                    input.run_id.0,
                    input.thread_id.0,
                    input.correlation_id,
                    result_json,
                    input.available_at_ms.map(|time| time as i64)
                ],
            )
            .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                p,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
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
        let current: Option<(u64, Option<String>)> = self
            .with_conn_unlocked(move |conn, p| {
                conn.query_row(
                    &format!("SELECT lease_epoch, lease_owner FROM {p}_dispatch WHERE run_id = ?1"),
                    params![run],
                    |row| Ok((row.get::<_, i64>(0)?.max(0) as u64, row.get(1)?)),
                )
                .optional()
                .map_err(reject)
            })
            .await?;
        Ok((current == Some((claim.epoch, Some(claim.owner.clone()))))
            .then(|| CommitEpochGuard::new(guard)))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // Priority: recover an expired lease, then wake an awaiting run with
            // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
            // the IMMEDIATE transaction is the single-owner guard.
            let recovery = format!(
                "SELECT run_id, request, sandbox FROM {p}_dispatch \
                 WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < ?1 \
                 ORDER BY created_at LIMIT 1"
            );
            // Single-writer-per-thread (ADR-0022): a wake or fresh pick skips any
            // thread that already has a run in flight. Recovery (above) is exempt —
            // it re-owns the SAME running row rather than adding a second.
            let not_running = format!(
                "NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
                 WHERE r.thread_id = d.thread_id AND r.status = 'running')"
            );
            let wake = format!(
                "SELECT run_id, request, sandbox FROM {p}_dispatch d \
                 WHERE d.status = 'awaiting' AND EXISTS ( \
                     SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                     AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                 AND {not_running} \
                 ORDER BY d.created_at LIMIT 1"
            );
            let fresh = format!(
                "SELECT run_id, request, sandbox FROM {p}_dispatch d \
                 WHERE d.status = 'pending' AND {not_running} \
                 ORDER BY d.priority DESC, d.created_at LIMIT 1"
            );

            let row = |sql: &str,
                       bind_now: bool|
             -> Result<Option<(String, String, Option<String>)>, DispatchError> {
                let map = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?));
                if bind_now {
                    tx.query_row(sql, params![now_ms as i64], map)
                } else {
                    tx.query_row(sql, [], map)
                }
                .optional()
                .map_err(reject)
            };

            // A recovery pick (expired-lease running row) spends one crash-retry.
            let mut recovery_pick = true;
            let picked = match row(&recovery, true)? {
                Some(found) => Some(found),
                None => {
                    recovery_pick = false;
                    match row(&wake, true)? {
                        Some(found) => Some(found),
                        None => row(&fresh, false)?,
                    }
                }
            };

            let Some((run_id, request_json, sandbox)) = picked else {
                return Ok(None);
            };
            let request: RunDispatch =
                serde_json::from_str(&request_json).map_err(json_err)?;

            let expires = now_ms + lease_ms;
            // Bump the fence token on every claim (fresh, wake, recovery); read it
            // back within the same IMMEDIATE tx so the lease carries the epoch the
            // holder will settle under.
            tx.execute(
                &format!(
                    "UPDATE {p}_dispatch SET status = 'running', lease_owner = ?1, \
                     lease_until = ?2, attempt_count = attempt_count + ?3, \
                     lease_epoch = lease_epoch + 1, worker_assignment = NULL WHERE run_id = ?4"
                ),
                params![owner, expires as i64, i64::from(recovery_pick), run_id],
            )
            .map_err(reject)?;
            let lease_epoch: i64 = tx
                .query_row(
                    &format!("SELECT lease_epoch FROM {p}_dispatch WHERE run_id = ?1"),
                    params![run_id],
                    |r| r.get(0),
                )
                .map_err(reject)?;

            // Hand the run's current pending input to the worker (not removed
            // here: settle removes exactly what the worker reports it consumed).
            let pending = {
                let mut stmt = tx
                    .prepare(&format!(
                        "SELECT message_id, thread_id, correlation_id, result, available_at \
                         FROM {p}_pending \
                         WHERE run_id = ?1 AND (available_at IS NULL OR available_at <= ?2) \
                         ORDER BY created_at"
                    ))
                    .map_err(reject)?;
                let rows = stmt
                    .query_map(params![run_id, now_ms as i64], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ))
                    })
                    .map_err(reject)?;
                let mut pending = Vec::new();
                for row in rows {
                    let (message_id, thread_id, correlation_id, result, available_at) =
                        row.map_err(reject)?;
                    pending.push(PendingInput {
                        message_id,
                        run_id: RunId(run_id.clone()),
                        thread_id: ThreadId(thread_id),
                        correlation_id,
                        available_at_ms: available_at.map(|t| t as u64),
                        result: serde_json::from_str(&result).map_err(json_err)?,
                    });
                }
                pending
            };

            tx.commit().map_err(reject)?;

            Ok(Some(Claimed {
                request,
                lease: Lease {
                    run_id: RunId(run_id),
                    owner,
                    expires_ms: expires,
                    epoch: lease_epoch as u64,
                },
                pending,
                recovered: recovery_pick,
                sandbox,
                assignment: None,
            }))
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
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment FROM {p}_dispatch d WHERE \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r WHERE r.thread_id = d.thread_id AND r.status = 'running')) OR \
                 (d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM {p}_dispatch r \
                   WHERE r.thread_id = d.thread_id AND r.status = 'running')) \
                 ORDER BY CASE WHEN d.status = 'running' THEN 0 WHEN d.status = 'awaiting' THEN 1 ELSE 2 END, \
                          d.priority DESC, d.created_at"
            );
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(reject)?;
                let rows = stmt
                    .query_map(params![now_ms as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })
                    .map_err(reject)?;
                let mut selected = None;
                for row in rows {
                    let (run_id, request_json, sandbox, previous_json) = row.map_err(reject)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    if can_assign(
                        &worker,
                        &request.placement,
                        previous.as_ref(),
                        sandbox.is_some(),
                        now_ms,
                    )
                    .is_ok()
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
                p,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed =
                claim_exact_transaction(&tx, p, &requested_run, &owner, lease_ms, now_ms, None)?;
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
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                p,
                &requested_run,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
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
                    params![(now_ms + lease_ms) as i64, run_id, owner],
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

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        self.with_conn(move |conn, p| {
            let depth: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
                         d.status = 'pending' OR \
                         (d.status = 'running' AND d.lease_until < ?1) OR \
                         (d.status = 'awaiting' AND EXISTS (\
                           SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
                           AND (i.available_at IS NULL OR i.available_at <= ?1)\
                         ))"
                    ),
                    params![now_ms as i64],
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
                        (now_ms + lease_ms) as i64,
                        owner,
                        (now_ms + lease_ms / 2) as i64
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
            // Fence first: mutate the dispatch row ONLY while the caller still holds
            // the current epoch. A stale owner (lower epoch) affects zero rows, so
            // its settle touches neither the dispatch nor its pending.
            let dispatch_rows = match outcome {
                DispatchOutcome::Done => tx
                    .execute(
                        &format!("DELETE FROM {p}_dispatch WHERE run_id = ?1 AND lease_epoch = ?2"),
                        params![run_id, epoch as i64],
                    )
                    .map_err(reject)?,
                DispatchOutcome::Awaiting => tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = 'awaiting', lease_owner = NULL, \
                             lease_until = NULL, attempt_count = 0 \
                             WHERE run_id = ?1 AND lease_epoch = ?2"
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
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET status = 'dead_letter', lease_owner = NULL, \
                         lease_until = NULL, dead_lettered_at = ?1 WHERE status = 'running' \
                         AND lease_until IS NOT NULL AND lease_until < ?1 AND attempt_count >= ?2"
                    ),
                    params![now_ms as i64, max_attempts as i64],
                )
                .map_err(reject)?;
            Ok(n)
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
                    "SELECT run_id, thread_id, status, attempt_count FROM {p}_dispatch \
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
                    ))
                })
                .map_err(reject)?;
            let mut out = Vec::new();
            for row in rows {
                let (run_id, thread_id, status, attempt_count) = row.map_err(reject)?;
                let state = DispatchState::from_db(&status).ok_or_else(|| {
                    DispatchError::Rejected(format!("unknown persisted dispatch state {status}"))
                })?;
                out.push(DispatchSummary {
                    run_id: RunId(run_id),
                    thread_id: ThreadId(thread_id),
                    state,
                    attempt_count: attempt_count as u64,
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
            let thread: Option<String> = tx
                .query_row(
                    &format!(
                        "SELECT thread_id FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status IN ('pending', 'awaiting')"
                    ),
                    params![run_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(reject)?;
            if thread.is_some() {
                tx.execute(
                    &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                    params![run_id],
                )
                .map_err(reject)?;
                tx.execute(
                    &format!("DELETE FROM {p}_dispatch WHERE run_id = ?1"),
                    params![run_id],
                )
                .map_err(reject)?;
            }
            tx.commit().map_err(reject)?;
            Ok(thread.map(ThreadId))
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
                params![cutoff_ms as i64],
            )
            .map_err(reject)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE {cond}"),
                    params![cutoff_ms as i64],
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
        let result_json = json(&input.result)?;
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_pending \
                         (message_id, run_id, thread_id, correlation_id, result, available_at) \
                         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![
                        input.message_id,
                        input.run_id.0,
                        input.thread_id.0,
                        input.correlation_id,
                        result_json,
                        input.available_at_ms.map(|t| t as i64)
                    ],
                )
                .map_err(reject)?;
            Ok(changed > 0)
        })
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
                        available_at_ms: available_at.map(|t| t as u64),
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
impl Outbox for SqliteDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let payload = json(&input)?;
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_outbox (message_id, payload) VALUES (?1,?2) \
                         ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![input.message_id, payload],
                )
                .map_err(reject)?;
            Ok(changed > 0)
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
                let input: PendingInput = serde_json::from_str(&payload).map_err(json_err)?;
                let result_json = json(&input.result)?;
                // One transaction per message: idempotent target append, then
                // drop the outbox row.
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(reject)?;
                tx.execute(
                    &format!(
                        "INSERT INTO {p}_pending \
                         (message_id, run_id, thread_id, correlation_id, result, available_at) \
                         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![
                        input.message_id,
                        input.run_id.0,
                        input.thread_id.0,
                        input.correlation_id,
                        result_json,
                        input.available_at_ms.map(|t| t as i64)
                    ],
                )
                .map_err(reject)?;
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
        .query_map(params![run_id, now_ms as i64], |row| {
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
            available_at_ms: available_at.map(|time| time as u64),
            result: serde_json::from_str(&result).map_err(json_err)?,
        });
    }
    Ok(pending)
}

fn claim_exact_transaction(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
) -> Result<Option<Claimed>, DispatchError> {
    let not_running = format!(
        "NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.status = 'running')"
    );
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment FROM {prefix}_dispatch d \
         WHERE d.run_id = ?1 AND ( \
           (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?2) \
           OR (d.status = 'awaiting' AND EXISTS ( \
             SELECT 1 FROM {prefix}_pending pe WHERE pe.run_id = d.run_id \
             AND (pe.available_at IS NULL OR pe.available_at <= ?2)) AND {not_running}) \
           OR (d.status = 'pending' AND {not_running}) \
         ) LIMIT 1"
    );
    let picked: Option<(String, Option<String>, String, Option<String>)> = tx
        .query_row(&sql, params![requested_run, now_ms as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()
        .map_err(reject)?;
    let Some((request_json, sandbox, status, previous_json)) = picked else {
        return Ok(None);
    };
    let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
    let previous: Option<WorkerAssignment> = previous_json
        .map(|value| serde_json::from_str(&value).map_err(json_err))
        .transpose()?;
    if let Some(worker) = worker
        && can_assign(
            worker,
            &request.placement,
            previous.as_ref(),
            sandbox.is_some(),
            now_ms,
        )
        .is_err()
    {
        return Ok(None);
    }
    let expires = now_ms + lease_ms;
    tx.execute(
        &format!(
            "UPDATE {prefix}_dispatch SET status = 'running', lease_owner = ?1, \
             lease_until = ?2, attempt_count = attempt_count + ?3, \
             lease_epoch = lease_epoch + 1, worker_assignment = ?5 WHERE run_id = ?4"
        ),
        params![
            owner,
            expires as i64,
            i64::from(status == "running"),
            requested_run,
            worker
                .map(WorkerAssignment::from)
                .map(|value| json(&value))
                .transpose()?
        ],
    )
    .map_err(reject)?;
    let lease_epoch: i64 = tx
        .query_row(
            &format!("SELECT lease_epoch FROM {prefix}_dispatch WHERE run_id = ?1"),
            params![requested_run],
            |row| row.get(0),
        )
        .map_err(reject)?;
    Ok(Some(Claimed {
        request,
        lease: Lease {
            run_id: RunId(requested_run.to_string()),
            owner: owner.to_string(),
            expires_ms: expires,
            epoch: lease_epoch as u64,
        },
        pending: pending_for_run(tx, prefix, requested_run, now_ms)?,
        recovered: status == "running",
        sandbox,
        assignment: worker.map(WorkerAssignment::from),
    }))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, DispatchError> {
    serde_json::to_string(value).map_err(json_err)
}

fn json_err(err: serde_json::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}

fn reject(err: rusqlite::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
