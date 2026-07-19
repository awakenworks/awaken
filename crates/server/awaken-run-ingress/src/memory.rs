//! In-memory reference implementation of the durable-ingress store.
//!
//! It mirrors the Postgres store's behaviour exactly so the worker and ingress
//! can be tested without a database (the same role [`MemoryCommitCoordinator`]
//! plays for the commit boundary). It is the executable specification of the
//! claim/lease/wake/recovery rules; the Postgres store must match it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{WorkerAssignment, WorkerSnapshot};
use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchState, DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim,
    SettleOutcome, SubmitOptions,
};
use awaken_run_ingress_contract::RunDispatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    Pending,
    Leased,
    Awaiting,
    DeadLetter,
    Superseded,
}

impl RowState {
    fn public(self) -> DispatchState {
        match self {
            RowState::Pending => DispatchState::Pending,
            RowState::Leased => DispatchState::Leased,
            RowState::Awaiting => DispatchState::Awaiting,
            RowState::DeadLetter => DispatchState::DeadLetter,
            RowState::Superseded => DispatchState::Superseded,
        }
    }
}

#[derive(Debug, Clone)]
struct Row {
    request: RunDispatch,
    state: RowState,
    lease: Option<Lease>,
    /// Consecutive crash-recoveries without a settle; reset when the run awaits.
    attempt_count: u64,
    priority: i64,
    epoch: i64,
    /// Monotone fence token bumped on every claim; the lease carries it and settle
    /// fences on it (mirrors the SQL backends' `lease_epoch` column).
    lease_epoch: u64,
    dedupe_key: Option<String>,
    /// When the run was dead-lettered (epoch ms), for time-windowed GC.
    dead_lettered_at: Option<u64>,
    /// Opaque sandbox binding (B-P3, ADR-0021 §6); set by `bind_sandbox`, returned
    /// on `claim` so recovery re-adopts the same sandbox.
    sandbox: Option<String>,
    assignment: Option<WorkerAssignment>,
}

/// A pending input with its optimistic-concurrency revision.
#[derive(Debug, Clone)]
struct PendingRow {
    input: PendingInput,
    revision: u64,
}

#[derive(Debug, Default)]
struct State {
    /// Enqueue order, so claim is deterministic (oldest first).
    order: Vec<RunId>,
    rows: HashMap<RunId, Row>,
    /// Undelivered pending input, in arrival order.
    pending: Vec<PendingRow>,
    /// Cross-thread deliveries staged for relay, in arrival order.
    outbox: Vec<PendingInput>,
}

/// In-memory durable-ingress store. Cloneable handles share one state.
#[derive(Debug)]
pub struct MemoryDispatchStore {
    state: Mutex<State>,
    authority: Arc<tokio::sync::Mutex<()>>,
}

impl Default for MemoryDispatchStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            authority: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl MemoryDispatchStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live dispatch rows (test introspection).
    pub fn dispatch_count(&self) -> usize {
        self.state.lock().map(|s| s.rows.len()).unwrap_or(0)
    }

    /// Remove dead-lettered rows matching `keep` (and their pending), returning
    /// the count — shared by the unconditional and time-windowed GC.
    fn purge_dead(&self, keep: impl Fn(&Row) -> bool) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        let dead: Vec<RunId> = state
            .rows
            .iter()
            .filter(|(_, row)| row.state == RowState::DeadLetter && keep(row))
            .map(|(run, _)| run.clone())
            .collect();
        for run in &dead {
            state.rows.remove(run);
            state.order.retain(|r| r != run);
            state.pending.retain(|p| &p.input.run_id != run);
        }
        Ok(dead.len())
    }

    /// Unconsumed pending inputs for a run (test introspection).
    pub fn pending_count(&self, run_id: &RunId) -> usize {
        self.state
            .lock()
            .map(|s| {
                s.pending
                    .iter()
                    .filter(|p| &p.input.run_id == run_id)
                    .count()
            })
            .unwrap_or(0)
    }
}

fn lock(state: &Mutex<State>) -> Result<std::sync::MutexGuard<'_, State>, DispatchError> {
    state
        .lock()
        .map_err(|_| DispatchError::Rejected("dispatch store poisoned".to_string()))
}

/// A pending input is deliverable when it has no schedule or its time has come.
fn is_due(input: &PendingInput, now_ms: u64) -> bool {
    input.available_at_ms.is_none_or(|t| t <= now_ms)
}

/// Pick the next runnable run, oldest-first within each priority band: reclaim an
/// expired lease (recovery), then wake an awaiting run with pending input, then a
/// fresh pending run. This is the claim policy the Postgres store must match.
fn select_where(
    state: &State,
    now_ms: u64,
    compatible: impl Fn(&RunDispatch) -> bool,
) -> Option<RunId> {
    // Single-writer-per-thread (ADR-0022): a wake or fresh pick must not start a
    // second concurrent run for a thread that already has one running. A recovery
    // pick is exempt — it re-owns the SAME running row, it does not add a second.
    let thread_running = |thread: &ThreadId| -> bool {
        state
            .rows
            .values()
            .any(|r| r.state == RowState::Leased && r.request.thread_id() == thread)
    };

    // Recovery: re-own an expired-lease running row (first-match in enqueue order).
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == RowState::Leased
            && row.lease.as_ref().is_some_and(|l| l.expires_ms < now_ms)
            && compatible(&row.request)
        {
            return Some(run.clone());
        }
    }
    // Wake: an awaiting run with due input whose thread is not already running.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == RowState::Awaiting
            && state
                .pending
                .iter()
                .any(|p| &p.input.run_id == run && is_due(&p.input, now_ms))
            && !thread_running(row.request.thread_id())
            && compatible(&row.request)
        {
            return Some(run.clone());
        }
    }
    // Fresh work is ordered by priority (highest first), then enqueue order, and
    // only for threads that are not already running.
    let mut best: Option<(&RunId, i64)> = None;
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == RowState::Pending
            && !thread_running(row.request.thread_id())
            && compatible(&row.request)
            && best.is_none_or(|(_, p)| row.priority > p)
        {
            best = Some((run, row.priority));
        }
    }
    best.map(|(run, _)| run.clone())
}

fn select(state: &State, now_ms: u64) -> Option<RunId> {
    select_where(state, now_ms, |_| true)
}

/// Whether one exact row is runnable under the same policy as [`select`]. The
/// boolean says that the claim is crash recovery and must spend retry budget.
fn runnable(state: &State, run_id: &RunId, now_ms: u64) -> Option<bool> {
    let row = state.rows.get(run_id)?;
    if row.state == RowState::Leased
        && row
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_ms < now_ms)
    {
        return Some(true);
    }
    let thread_busy = state.rows.values().any(|candidate| {
        candidate.state == RowState::Leased
            && candidate.request.thread_id() == row.request.thread_id()
    });
    if thread_busy {
        return None;
    }
    if row.state == RowState::Awaiting
        && state
            .pending
            .iter()
            .any(|pending| pending.input.run_id == *run_id && is_due(&pending.input, now_ms))
    {
        return Some(false);
    }
    (row.state == RowState::Pending).then_some(false)
}

fn claim_exact(
    state: &mut State,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    assignment: Option<WorkerAssignment>,
) -> Option<Claimed> {
    let was_recovery = runnable(state, requested_run, now_ms)?;
    let run_id = requested_run.clone();
    let (request, sandbox, lease) = {
        let row = state.rows.get_mut(&run_id).expect("runnable row exists");
        row.lease_epoch += 1;
        let lease = Lease {
            run_id: run_id.clone(),
            owner: owner.to_string(),
            expires_ms: now_ms + lease_ms,
            epoch: row.lease_epoch,
        };
        row.state = RowState::Leased;
        row.lease = Some(lease.clone());
        row.assignment = assignment.clone();
        if was_recovery {
            row.attempt_count += 1;
        }
        (row.request.clone(), row.sandbox.clone(), lease)
    };
    let pending = state
        .pending
        .iter()
        .filter(|pending| pending.input.run_id == run_id && is_due(&pending.input, now_ms))
        .map(|pending| pending.input.clone())
        .collect();
    Some(Claimed {
        request,
        lease,
        pending,
        recovered: was_recovery,
        sandbox,
        assignment,
    })
}

#[async_trait]
impl DispatchQueue for MemoryDispatchStore {
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let state = lock(&self.state)?;
        let matches = state.rows.get(&claim.run_id).is_some_and(|row| {
            row.lease_epoch == claim.epoch
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.owner == claim.owner)
        });
        drop(state);
        Ok(matches.then(|| CommitEpochGuard::new(guard)))
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        // Idempotent by run id; and a no-op while a live dispatch shares the
        // caller's dedupe key.
        if state.rows.contains_key(&run_id) {
            return Ok(());
        }
        if let Some(key) = &options.dedupe_key
            && state
                .rows
                .values()
                .any(|r| r.dedupe_key.as_deref() == Some(key) && r.state != RowState::DeadLetter)
        {
            return Ok(());
        }
        // Supersession: take the highest epoch on the thread and mark its prior
        // pending/awaiting work superseded — the newest submission wins (ADR-0022).
        let thread = request.thread_id().clone();
        let mut epoch = 0;
        if options.supersede {
            epoch = state
                .rows
                .values()
                .filter(|r| *r.request.thread_id() == thread)
                .map(|r| r.epoch)
                .max()
                .unwrap_or(0)
                + 1;
            for row in state.rows.values_mut() {
                if *row.request.thread_id() == thread
                    && matches!(row.state, RowState::Pending | RowState::Awaiting)
                {
                    row.state = RowState::Superseded;
                    row.lease = None;
                }
            }
        }
        state.rows.insert(
            run_id.clone(),
            Row {
                request,
                state: RowState::Pending,
                lease: None,
                attempt_count: 0,
                priority: options.priority,
                epoch,
                lease_epoch: 0,
                dedupe_key: options.dedupe_key,
                dead_lettered_at: None,
                sandbox: None,
                assignment: None,
            },
        );
        state.order.push(run_id);
        Ok(())
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        if !state.rows.contains_key(&run_id) {
            state.rows.insert(
                run_id.clone(),
                Row {
                    request,
                    state: RowState::Pending,
                    lease: None,
                    attempt_count: 0,
                    priority: 0,
                    epoch: 0,
                    lease_epoch: 0,
                    dedupe_key: None,
                    dead_lettered_at: None,
                    sandbox: None,
                    assignment: None,
                },
            );
            state.order.push(run_id.clone());
        }
        Ok(claim_exact(
            &mut state, &run_id, owner, lease_ms, now_ms, None,
        ))
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        if !worker.accepts(&request.placement, now_ms) {
            return Ok(None);
        }
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        if !state.rows.contains_key(&run_id) {
            state.rows.insert(
                run_id.clone(),
                Row {
                    request,
                    state: RowState::Pending,
                    lease: None,
                    attempt_count: 0,
                    priority: 0,
                    epoch: 0,
                    lease_epoch: 0,
                    dedupe_key: None,
                    dead_lettered_at: None,
                    sandbox: None,
                    assignment: None,
                },
            );
            state.order.push(run_id.clone());
        }
        Ok(claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(WorkerAssignment::from(worker)),
        ))
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = input.run_id.clone();
        if !state
            .pending
            .iter()
            .any(|pending| pending.input.message_id == input.message_id)
        {
            state.pending.push(PendingRow { input, revision: 1 });
        }
        Ok(claim_exact(
            &mut state, &run_id, owner, lease_ms, now_ms, None,
        ))
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = input.run_id.clone();
        if !state
            .pending
            .iter()
            .any(|pending| pending.input.message_id == input.message_id)
        {
            state.pending.push(PendingRow { input, revision: 1 });
        }
        if state
            .rows
            .get(&run_id)
            .is_none_or(|row| !worker.accepts(&row.request.placement, now_ms))
        {
            return Ok(None);
        }
        Ok(claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(WorkerAssignment::from(worker)),
        ))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;

        let Some(run_id) = select(&state, now_ms) else {
            return Ok(None);
        };

        // A recovery pick (an expired-lease running row) spends one crash-retry;
        // a fresh or wake pick does not.
        let was_recovery = matches!(
            state.rows.get(&run_id).map(|r| r.state),
            Some(RowState::Leased)
        );
        let (request, sandbox, lease) = {
            let row = state.rows.get_mut(&run_id).expect("picked row exists");
            // Bump the fence token on every claim; the lease carries the new epoch.
            row.lease_epoch += 1;
            let lease = Lease {
                run_id: run_id.clone(),
                owner: owner.to_string(),
                expires_ms: now_ms + lease_ms,
                epoch: row.lease_epoch,
            };
            row.state = RowState::Leased;
            row.lease = Some(lease.clone());
            row.assignment = None;
            if was_recovery {
                row.attempt_count += 1;
            }
            (row.request.clone(), row.sandbox.clone(), lease)
        };

        // Hand the run's current pending input to the worker. It is not removed
        // here: settle removes exactly what the worker reports it consumed, so a
        // crash before settle leaves the input to be re-derived (ADR-0010).
        let pending = state
            .pending
            .iter()
            .filter(|p| p.input.run_id == run_id && is_due(&p.input, now_ms))
            .map(|p| p.input.clone())
            .collect();

        Ok(Some(Claimed {
            request,
            lease,
            pending,
            recovered: was_recovery,
            sandbox,
            assignment: None,
        }))
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(run_id) = select_where(&state, now_ms, |request| {
            worker.accepts(&request.placement, now_ms)
        }) else {
            return Ok(None);
        };
        Ok(claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(WorkerAssignment::from(worker)),
        ))
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        Ok(claim_exact(
            &mut state,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
        ))
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        if state
            .rows
            .get(requested_run)
            .is_none_or(|row| !worker.accepts(&row.request.placement, now_ms))
        {
            return Ok(None);
        }
        Ok(claim_exact(
            &mut state,
            requested_run,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(WorkerAssignment::from(worker)),
        ))
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        if let Some(row) = state.rows.get_mut(&claim.run_id)
            && row.lease_epoch == claim.epoch
            && row
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner == claim.owner)
        {
            row.sandbox = Some(sandbox_ref.to_string());
            return Ok(SettleOutcome::Applied);
        }
        Ok(SettleOutcome::Fenced)
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        let state = lock(&self.state)?;
        let depth = state
            .rows
            .keys()
            .filter(|run_id| runnable(&state, run_id, now_ms).is_some())
            .count() as u64;
        Ok(Some(depth))
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        match state.rows.get_mut(run_id) {
            Some(row)
                if row.state == RowState::Leased
                    && row.lease.as_ref().is_some_and(|l| l.owner == owner) =>
            {
                if let Some(lease) = row.lease.as_mut() {
                    lease.expires_ms = now_ms + lease_ms;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        let mut renewed = 0;
        // Only rows within half a lease of expiring; a fresh claim is a full length
        // out and is skipped until it approaches expiry (ADR-0024).
        let near_expiry = now_ms + lease_ms / 2;
        for row in state.rows.values_mut() {
            if row.state == RowState::Leased
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.owner == owner && l.expires_ms < near_expiry)
            {
                if let Some(lease) = row.lease.as_mut() {
                    lease.expires_ms = now_ms + lease_ms;
                }
                renewed += 1;
            }
        }
        Ok(renewed)
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        // Fence: apply only while the caller still holds the current epoch. A stale
        // owner (lower epoch, or a gone row) changes nothing — the reclaimer's
        // in-flight state is inviolate.
        let current = state.rows.get(run_id).map(|r| r.lease_epoch);
        if current != Some(epoch) {
            return Ok(SettleOutcome::Fenced);
        }
        match outcome {
            DispatchOutcome::Done => {
                state.rows.remove(run_id);
                state.order.retain(|r| r != run_id);
                // Drop the run's own pending and anything else the worker consumed
                // this attempt (e.g. unbound idle-thread input, ADR-0021).
                state.pending.retain(|p| {
                    &p.input.run_id != run_id && !consumed.contains(&p.input.message_id)
                });
            }
            DispatchOutcome::Awaiting => {
                if let Some(row) = state.rows.get_mut(run_id) {
                    row.state = RowState::Awaiting;
                    row.lease = None;
                    // Reaching a checkpoint refreshes the crash-retry budget.
                    row.attempt_count = 0;
                }
                // Drop only what the worker consumed; input that arrived during
                // the attempt stays for the next wake.
                state
                    .pending
                    .retain(|p| !consumed.contains(&p.input.message_id));
            }
        }
        Ok(SettleOutcome::Applied)
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut reaped = 0;
        for row in state.rows.values_mut() {
            let expired = row.state == RowState::Leased
                && row.lease.as_ref().is_some_and(|l| l.expires_ms < now_ms);
            if expired && row.attempt_count >= max_attempts {
                row.state = RowState::DeadLetter;
                row.lease = None;
                row.dead_lettered_at = Some(now_ms);
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .filter(|run| {
                matches!(
                    state.rows.get(run).map(|r| r.state),
                    Some(RowState::DeadLetter)
                )
            })
            .cloned()
            .collect())
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .filter(|run| {
                matches!(
                    state.rows.get(run).map(|r| r.state),
                    Some(RowState::Superseded)
                )
            })
            .cloned()
            .collect())
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .filter_map(|run| {
                state.rows.get(run).map(|row| DispatchSummary {
                    run_id: run.clone(),
                    thread_id: row.request.thread_id().clone(),
                    state: row.state.public(),
                    attempt_count: row.attempt_count,
                })
            })
            .collect())
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        match state.rows.get_mut(run_id) {
            Some(row) if row.state == RowState::DeadLetter => {
                row.state = RowState::Pending;
                row.lease = None;
                row.attempt_count = 0;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let cancellable = matches!(
            state.rows.get(run_id).map(|r| r.state),
            Some(RowState::Pending | RowState::Awaiting)
        );
        if !cancellable {
            return Ok(None);
        }
        let thread = state.rows[run_id].request.thread_id().clone();
        state.rows.remove(run_id);
        state.order.retain(|r| r != run_id);
        state.pending.retain(|p| &p.input.run_id != run_id);
        Ok(Some(thread))
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .find(|run| {
                state.rows.get(*run).is_some_and(|row| {
                    row.state == RowState::Awaiting && row.request.thread_id() == thread_id
                })
            })
            .cloned())
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.purge_dead(|_| true)
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.purge_dead(|row| row.dead_lettered_at.is_some_and(|at| at <= cutoff_ms))
    }
}

#[async_trait]
impl Inbox for MemoryDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        if state
            .pending
            .iter()
            .any(|p| p.input.message_id == input.message_id)
        {
            return Ok(false);
        }
        state.pending.push(PendingRow { input, revision: 1 });
        Ok(true)
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .pending
            .iter()
            .filter(|p| &p.input.thread_id == thread_id)
            .map(|p| PendingRecord {
                input: p.input.clone(),
                revision: p.revision,
            })
            .collect())
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(pos) = state
            .pending
            .iter()
            .position(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if state.pending[pos].revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        state.pending.remove(pos);
        Ok(CasOutcome::Applied)
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(row) = state
            .pending
            .iter_mut()
            .find(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if row.revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        row.input.result = result;
        row.revision += 1;
        Ok(CasOutcome::Applied)
    }
}

#[async_trait]
impl Outbox for MemoryDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        if state
            .outbox
            .iter()
            .any(|i| i.message_id == input.message_id)
        {
            return Ok(false);
        }
        state.outbox.push(input);
        Ok(true)
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        let staged = std::mem::take(&mut state.outbox);
        let relayed = staged.len();
        for input in staged {
            // Idempotent target append: skip a message already pending.
            if !state
                .pending
                .iter()
                .any(|p| p.input.message_id == input.message_id)
            {
                state.pending.push(PendingRow { input, revision: 1 });
            }
        }
        Ok(relayed)
    }
}
