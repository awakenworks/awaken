//! In-memory reference implementation of the durable-ingress store.
//!
//! It mirrors the Postgres store's behaviour exactly so the worker and ingress
//! can be tested without a database (the same role [`MemoryCommitCoordinator`]
//! plays for the commit boundary). It is the executable specification of the
//! claim/lease/wake/recovery rules; the Postgres store must match it.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    CasOutcome, Claimed, DispatchError, DispatchOutcome, DispatchQueue, DispatchStatus,
    DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, SubmitOptions,
};
use crate::request::RunExecutionRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Parked,
    DeadLetter,
    Superseded,
}

impl Status {
    fn public(self) -> DispatchStatus {
        match self {
            Status::Pending => DispatchStatus::Pending,
            Status::Running => DispatchStatus::Running,
            Status::Parked => DispatchStatus::Parked,
            Status::DeadLetter => DispatchStatus::DeadLetter,
            Status::Superseded => DispatchStatus::Superseded,
        }
    }
}

#[derive(Debug, Clone)]
struct Row {
    request: RunExecutionRequest,
    status: Status,
    lease: Option<Lease>,
    /// Consecutive crash-recoveries without a settle; reset when the run parks.
    attempt_count: u64,
    priority: i64,
    epoch: i64,
    dedupe_key: Option<String>,
    /// When the run was dead-lettered (epoch ms), for time-windowed GC.
    dead_lettered_at: Option<u64>,
    /// Opaque sandbox binding (B-P3, ADR-0021 §6); set by `bind_sandbox`, returned
    /// on `claim` so recovery re-adopts the same sandbox.
    sandbox: Option<String>,
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
#[derive(Debug, Default)]
pub struct MemoryDispatchStore {
    state: Mutex<State>,
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
            .filter(|(_, row)| row.status == Status::DeadLetter && keep(row))
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
/// expired lease (recovery), then wake a parked run with pending input, then a
/// fresh pending run. This is the claim policy the Postgres store must match.
fn select(state: &State, now_ms: u64) -> Option<RunId> {
    // Single-writer-per-thread (ADR-0022): a wake or fresh pick must not start a
    // second concurrent run for a thread that already has one running. A recovery
    // pick is exempt — it re-owns the SAME running row, it does not add a second.
    let thread_running = |thread: &ThreadId| -> bool {
        state
            .rows
            .values()
            .any(|r| r.status == Status::Running && r.request.thread_id() == thread)
    };

    // Recovery: re-own an expired-lease running row (first-match in enqueue order).
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.status == Status::Running
            && row.lease.as_ref().is_some_and(|l| l.expires_ms <= now_ms)
        {
            return Some(run.clone());
        }
    }
    // Wake: a parked run with due input whose thread is not already running.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.status == Status::Parked
            && state
                .pending
                .iter()
                .any(|p| &p.input.run_id == run && is_due(&p.input, now_ms))
            && !thread_running(row.request.thread_id())
        {
            return Some(run.clone());
        }
    }
    // Fresh work is ordered by priority (highest first), then enqueue order, and
    // only for threads that are not already running.
    let mut best: Option<(&RunId, i64)> = None;
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.status == Status::Pending
            && !thread_running(row.request.thread_id())
            && best.is_none_or(|(_, p)| row.priority > p)
        {
            best = Some((run, row.priority));
        }
    }
    best.map(|(run, _)| run.clone())
}

#[async_trait]
impl DispatchQueue for MemoryDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunExecutionRequest,
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
                .any(|r| r.dedupe_key.as_deref() == Some(key) && r.status != Status::DeadLetter)
        {
            return Ok(());
        }
        // Supersession: take the highest epoch on the thread and mark its prior
        // pending/parked work superseded — the newest submission wins (ADR-0022).
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
                    && matches!(row.status, Status::Pending | Status::Parked)
                {
                    row.status = Status::Superseded;
                    row.lease = None;
                }
            }
        }
        state.rows.insert(
            run_id.clone(),
            Row {
                request,
                status: Status::Pending,
                lease: None,
                attempt_count: 0,
                priority: options.priority,
                epoch,
                dedupe_key: options.dedupe_key,
                dead_lettered_at: None,
                sandbox: None,
            },
        );
        state.order.push(run_id);
        Ok(())
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut state = lock(&self.state)?;

        let Some(run_id) = select(&state, now_ms) else {
            return Ok(None);
        };

        let lease = Lease {
            run_id: run_id.clone(),
            owner: owner.to_string(),
            expires_ms: now_ms + lease_ms,
        };
        // A recovery pick (an expired-lease running row) spends one crash-retry;
        // a fresh or wake pick does not.
        let was_recovery = matches!(
            state.rows.get(&run_id).map(|r| r.status),
            Some(Status::Running)
        );
        let (request, sandbox) = {
            let row = state.rows.get_mut(&run_id).expect("picked row exists");
            row.status = Status::Running;
            row.lease = Some(lease.clone());
            if was_recovery {
                row.attempt_count += 1;
            }
            (row.request.clone(), row.sandbox.clone())
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
            sandbox,
        }))
    }

    async fn bind_sandbox(&self, run_id: &RunId, sandbox_ref: &str) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        if let Some(row) = state.rows.get_mut(run_id) {
            row.sandbox = Some(sandbox_ref.to_string());
        }
        Ok(())
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
                if row.status == Status::Running
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
            if row.status == Status::Running
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
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
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
            DispatchOutcome::Parked => {
                if let Some(row) = state.rows.get_mut(run_id) {
                    row.status = Status::Parked;
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
        Ok(())
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        let mut reaped = 0;
        for row in state.rows.values_mut() {
            let expired = row.status == Status::Running
                && row.lease.as_ref().is_some_and(|l| l.expires_ms <= now_ms);
            if expired && row.attempt_count >= max_attempts {
                row.status = Status::DeadLetter;
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
                    state.rows.get(run).map(|r| r.status),
                    Some(Status::DeadLetter)
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
                    state.rows.get(run).map(|r| r.status),
                    Some(Status::Superseded)
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
                    status: row.status.public(),
                    attempt_count: row.attempt_count,
                })
            })
            .collect())
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        match state.rows.get_mut(run_id) {
            Some(row) if row.status == Status::DeadLetter => {
                row.status = Status::Pending;
                row.lease = None;
                row.attempt_count = 0;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let mut state = lock(&self.state)?;
        let cancellable = matches!(
            state.rows.get(run_id).map(|r| r.status),
            Some(Status::Pending | Status::Parked)
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

    async fn parked_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .find(|run| {
                state.rows.get(*run).is_some_and(|row| {
                    row.status == Status::Parked && row.request.thread_id() == thread_id
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
