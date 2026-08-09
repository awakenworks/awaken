//! In-memory reference implementation of the durable-ingress store.
//!
//! It mirrors the Postgres store's behaviour exactly so the worker and ingress
//! can be tested without a database (the in-memory commit coordinator
//! plays for the commit boundary). It is the executable specification of the
//! claim/lease/wake/recovery rules; the Postgres store must match it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{
    DispatchPlacement, PlacementPolicy, WorkerAssignment, WorkerSnapshot, can_assign,
    can_claim_locally, policy_selects_requester,
};
use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, Claimed, CommitEpochGuard, CredentialRealizationReceipt,
    DispatchCompletion, DispatchError, DispatchOutcome, DispatchQueue, DispatchState,
    DispatchSummary, ExactClaimMode, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim,
    SettleOutcome, SubmitOptions, can_admit_attempt_credentials,
    compile_attempt_credential_bindings, installed_worker_credential_capabilities,
    normalize_pending_millis, verify_credential_realization_receipt,
};
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
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
    /// Set before signalling a live attempt. It remains true across lease expiry
    /// and recovery until the worker commits Cancelled and settles Done.
    cancellation_requested: bool,
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
    /// Exact secret-free decisions frozen with the current lease epoch.
    credential_bindings: Vec<AttemptCredentialBinding>,
    /// Idempotent effect evidence written under the same owner/epoch fence.
    credential_receipts: Vec<CredentialRealizationReceipt>,
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
    /// Applied-Done facts, retained as permanent run-id tombstones (ADR-0060).
    completions: Vec<DispatchCompletion>,
    /// Dispatch-authority transitions in the same order as their mutations.
    operations: Vec<DispatchOperationalEvent>,
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

fn push_operation(state: &mut State, operation: DispatchOperation) {
    state.operations.push(DispatchOperationalEvent {
        cursor: DispatchCursor(state.operations.len() as u64 + 1),
        recorded_at_ms: Some(crate::clock::system_now_ms()),
        operation,
    });
}

/// A pending input is deliverable when it has no schedule or its time has come.
fn is_due(input: &PendingInput, now_ms: u64) -> bool {
    input.available_at_ms.is_none_or(|time| {
        crate::clock::normalize_millis(time) <= crate::clock::normalize_millis(now_ms)
    })
}

/// The one in-memory pending-input insert path. Every caller gets identical
/// idempotency conflict and time-normalization semantics.
fn append_pending(state: &mut State, input: PendingInput) -> Result<bool, DispatchError> {
    let input = normalize_pending_millis(input);
    if let Some(existing) = state
        .pending
        .iter()
        .find(|pending| pending.input.message_id == input.message_id)
    {
        return if existing.input == input {
            Ok(false)
        } else {
            Err(DispatchError::Rejected(format!(
                "idempotency key `{}` was reused with another pending-input payload",
                input.message_id
            )))
        };
    }
    state.pending.push(PendingRow { input, revision: 1 });
    Ok(true)
}

/// Pick the next runnable run, oldest-first within each priority band: reclaim an
/// expired lease (recovery), then wake an awaiting run with pending input, then a
/// fresh pending run. This is the claim policy the Postgres store must match.
fn select_where(
    state: &State,
    now_ms: u64,
    mut compatible: impl FnMut(&Row) -> bool,
) -> Option<RunId> {
    let now_ms = crate::clock::normalize_millis(now_ms);
    // Single-writer-per-thread (ADR-0022): a wake or fresh pick must not start a
    // second concurrent run for a thread that already has one running. A recovery
    // pick is exempt — it re-owns the SAME running row, it does not add a second.
    let thread_running = |thread: &ThreadId| -> bool {
        state
            .rows
            .values()
            .any(|r| r.state == RowState::Leased && r.request.thread_id() == thread)
    };

    // Cancellation is terminal control, not ordinary work. Once its owning lease
    // is free, claim it before wakes/fresh runs so a superseding run cannot overtake
    // the durable intent on the same thread.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.cancellation_requested
            && matches!(row.state, RowState::Pending | RowState::Awaiting)
            && !thread_running(row.request.thread_id())
        {
            return Some(run.clone());
        }
    }

    // Recovery: re-own an expired-lease running row (first-match in enqueue order).
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == RowState::Leased
            && row.lease.as_ref().is_some_and(|l| l.expires_ms < now_ms)
            && compatible(row)
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
            && compatible(row)
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
            && compatible(row)
            && best.is_none_or(|(_, p)| row.priority > p)
        {
            best = Some((run, row.priority));
        }
    }
    best.map(|(run, _)| run.clone())
}

/// Whether one exact row is runnable under the same policy as [`select_where`]. The
/// boolean says that the claim is crash recovery and must spend retry budget.
fn runnable(state: &State, run_id: &RunId, now_ms: u64) -> Option<bool> {
    let now_ms = crate::clock::normalize_millis(now_ms);
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
        && (row.cancellation_requested
            || state
                .pending
                .iter()
                .any(|pending| pending.input.run_id == *run_id && is_due(&pending.input, now_ms)))
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
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    claim_exact_with_mode(
        state,
        requested_run,
        owner,
        lease_ms,
        now_ms,
        worker,
        capabilities,
        ExactClaimMode::Runnable,
    )
}

#[allow(clippy::too_many_arguments)]
fn claim_exact_with_mode(
    state: &mut State,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let was_recovery = match mode {
        ExactClaimMode::Runnable => {
            let Some(was_recovery) = runnable(state, requested_run, now_ms) else {
                return Ok(None);
            };
            was_recovery
        }
        ExactClaimMode::TerminalRecovery => {
            let Some(row) = state.rows.get(requested_run) else {
                return Ok(None);
            };
            let expired_running = row.state == RowState::Leased
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_ms < now_ms);
            let thread_busy = state.rows.values().any(|candidate| {
                candidate.state == RowState::Leased
                    && candidate.request.thread_id() == row.request.thread_id()
            });
            let quiescent_awaiting =
                row.state == RowState::Awaiting && row.lease.is_none() && !thread_busy;
            if !quiescent_awaiting && !expired_running {
                return Ok(None);
            }
            expired_running
        }
    };
    let terminal_recovery = mode == ExactClaimMode::TerminalRecovery;
    let run_id = requested_run.clone();
    let row = state.rows.get(&run_id).expect("claimable row exists");
    if !terminal_recovery
        && worker.is_none()
        && !row.cancellation_requested
        && !can_claim_locally(&row.request.placement)
    {
        return Ok(None);
    }
    let previous =
        was_recovery.then(|| row.lease.clone().expect("a recovery has an expired lease"));
    let claim_epoch = row
        .lease_epoch
        .checked_add(1)
        .ok_or_else(|| DispatchError::Rejected("dispatch claim epoch exhausted".to_string()))?;
    let credential_bindings = if terminal_recovery || row.cancellation_requested {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&row.request, capabilities, claim_epoch, now_ms)
            .map_err(|error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            })?
    };
    let assignment = (!terminal_recovery)
        .then(|| worker.map(WorkerAssignment::from))
        .flatten();
    let (request, sandbox, cancellation_requested, lease) = {
        let row = state.rows.get_mut(&run_id).expect("runnable row exists");
        row.lease_epoch = claim_epoch;
        let lease = Lease {
            run_id: run_id.clone(),
            owner: owner.to_string(),
            expires_ms: crate::clock::deadline_millis(now_ms, lease_ms),
            epoch: row.lease_epoch,
        };
        row.state = RowState::Leased;
        row.lease = Some(lease.clone());
        row.assignment = assignment.clone();
        row.credential_bindings.clone_from(&credential_bindings);
        row.credential_receipts.clear();
        if was_recovery {
            row.attempt_count += 1;
        }
        (
            row.request.clone(),
            row.sandbox.clone(),
            row.cancellation_requested,
            lease,
        )
    };
    let pending = state
        .pending
        .iter()
        .filter(|pending| pending.input.run_id == run_id && is_due(&pending.input, now_ms))
        .map(|pending| pending.input.clone())
        .collect();
    let claimed = Claimed {
        request,
        lease: lease.clone(),
        credential_bindings,
        cancellation_requested,
        pending,
        recovered: was_recovery,
        sandbox,
        assignment,
    };
    if let Some(previous) = previous {
        let previous = RunClaim::from(&previous);
        let claim = RunClaim::from(&lease);
        push_operation(
            state,
            DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason: LeaseLossReason::Expired,
            },
        );
        push_operation(state, DispatchOperation::Reclaimed { previous, claim });
    } else {
        push_operation(
            state,
            DispatchOperation::Claimed {
                claim: RunClaim::from(&lease),
            },
        );
    }
    Ok(Some(claimed))
}

fn claim_new_local(
    state: &mut State,
    request: RunDispatch,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    if !can_claim_locally(&request.placement) {
        return Ok(None);
    }
    let run_id = request.run_id().clone();
    if state
        .completions
        .iter()
        .any(|completion| completion.run_id == run_id)
    {
        return Ok(None);
    }
    if !state.rows.contains_key(&run_id) {
        state.rows.insert(
            run_id.clone(),
            Row {
                request,
                state: RowState::Pending,
                cancellation_requested: false,
                lease: None,
                attempt_count: 0,
                priority: 0,
                epoch: 0,
                lease_epoch: 0,
                dedupe_key: None,
                dead_lettered_at: None,
                sandbox: None,
                assignment: None,
                credential_bindings: Vec::new(),
                credential_receipts: Vec::new(),
            },
        );
        state.order.push(run_id.clone());
    }
    claim_exact(state, &run_id, owner, lease_ms, now_ms, None, capabilities)
}

fn deliver_and_claim_local(
    state: &mut State,
    input: PendingInput,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let run_id = input.run_id.clone();
    append_pending(state, input)?;
    claim_exact(state, &run_id, owner, lease_ms, now_ms, None, capabilities)
}

fn claim_next_local(
    state: &mut State,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let Some(run_id) = select_where(state, now_ms, |row| {
        can_claim_locally(&row.request.placement)
    }) else {
        return Ok(None);
    };
    claim_exact(state, &run_id, owner, lease_ms, now_ms, None, capabilities)
}

#[async_trait]
impl DispatchQueue for MemoryDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let owner = identity.lease_owner();
        let state = lock(&self.state)?;
        Ok(state.rows.get(run_id).is_some_and(|row| {
            row.state == RowState::Leased
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.owner == owner && lease.expires_ms >= now_ms)
        }))
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let state = lock(&self.state)?;
        let guarded = state.rows.get(&claim.run_id).and_then(|row| {
            (row.lease_epoch == claim.epoch
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.owner == claim.owner))
            .then(|| {
                (
                    row.request.clone(),
                    row.lease
                        .as_ref()
                        .expect("matched claim has a lease")
                        .expires_ms,
                )
            })
        });
        drop(state);
        Ok(guarded.map(|(request, expires_ms)| CommitEpochGuard::new(guard, request, expires_ms)))
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
        if state.rows.contains_key(&run_id)
            || state
                .completions
                .iter()
                .any(|completion| completion.run_id == run_id)
        {
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
                    && !row.cancellation_requested
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
                cancellation_requested: false,
                lease: None,
                attempt_count: 0,
                priority: options.priority,
                epoch,
                lease_epoch: 0,
                dedupe_key: options.dedupe_key,
                dead_lettered_at: None,
                sandbox: None,
                assignment: None,
                credential_bindings: Vec::new(),
                credential_receipts: Vec::new(),
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
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_new_local(&mut state, request, owner, lease_ms, now_ms, capabilities)
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
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        if state
            .completions
            .iter()
            .any(|completion| completion.run_id == run_id)
        {
            return Ok(None);
        }
        if !state.rows.contains_key(&run_id) {
            state.rows.insert(
                run_id.clone(),
                Row {
                    request,
                    state: RowState::Pending,
                    cancellation_requested: false,
                    lease: None,
                    attempt_count: 0,
                    priority: 0,
                    epoch: 0,
                    lease_epoch: 0,
                    dedupe_key: None,
                    dead_lettered_at: None,
                    sandbox: None,
                    assignment: None,
                    credential_bindings: Vec::new(),
                    credential_receipts: Vec::new(),
                },
            );
            state.order.push(run_id.clone());
        }
        claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        deliver_and_claim_local(&mut state, input, owner, lease_ms, now_ms, capabilities)
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
        append_pending(&mut state, input)?;
        if state.rows.get(&run_id).is_none_or(|row| {
            !row.cancellation_requested
                && can_assign(
                    worker,
                    &row.request.placement,
                    row.assignment.as_ref(),
                    row.sandbox.is_some(),
                    now_ms,
                )
                .is_err()
        }) {
            return Ok(None);
        }
        claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_next_local(&mut state, owner, lease_ms, now_ms, capabilities)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let capabilities = installed_worker_credential_capabilities(worker)?;
        let Some(run_id) = select_where(&state, now_ms, |row| {
            row.cancellation_requested
                || (can_assign(
                    worker,
                    &row.request.placement,
                    row.assignment.as_ref(),
                    row.sandbox.is_some(),
                    now_ms,
                )
                .is_ok()
                    && row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                        can_admit_attempt_credentials(&row.request, &capabilities, epoch, now_ms)
                    }))
        }) else {
            return Ok(None);
        };
        claim_exact(
            &mut state,
            &run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &capabilities,
        )
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut policy_error = None;
        let capabilities = installed_worker_credential_capabilities(requester)?;
        let run_id = select_where(&state, now_ms, |row| {
            if row.cancellation_requested {
                return true;
            }
            match policy_selects_requester(
                &row.request,
                policy.as_ref(),
                DispatchPlacement {
                    recovered: row.state == RowState::Leased,
                    previous: row.assignment.as_ref(),
                    sandbox_bound: row.sandbox.is_some(),
                    requester: &requester.identity,
                    workers: &workers,
                    now_ms,
                },
            ) {
                Ok(selected) => {
                    selected
                        && row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                            can_admit_attempt_credentials(
                                &row.request,
                                &capabilities,
                                epoch,
                                now_ms,
                            )
                        })
                }
                Err(error) => {
                    policy_error = Some(error);
                    false
                }
            }
        });
        if let Some(error) = policy_error {
            return Err(error);
        }
        let Some(run_id) = run_id else {
            return Ok(None);
        };
        claim_exact(
            &mut state,
            &run_id,
            &requester.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(requester),
            &capabilities,
        )
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_exact(
            &mut state,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            capabilities,
        )
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_exact_with_mode(
            &mut state,
            requested_run,
            owner,
            lease_ms,
            now_ms,
            None,
            &Default::default(),
            ExactClaimMode::TerminalRecovery,
        )
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
        if state.rows.get(requested_run).is_none_or(|row| {
            !row.cancellation_requested
                && can_assign(
                    worker,
                    &row.request.placement,
                    row.assignment.as_ref(),
                    row.sandbox.is_some(),
                    now_ms,
                )
                .is_err()
        }) {
            return Ok(None);
        }
        claim_exact(
            &mut state,
            requested_run,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get_mut(&claim.run_id) else {
            return Ok(SettleOutcome::Fenced);
        };
        if row.state != RowState::Leased
            || row.lease_epoch != claim.epoch
            || row
                .lease
                .as_ref()
                .is_none_or(|lease| lease.owner != claim.owner)
        {
            return Ok(SettleOutcome::Fenced);
        }
        verify_credential_realization_receipt(&row.credential_bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        if let Some(existing) = row
            .credential_receipts
            .iter()
            .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint)
        {
            return if existing == &receipt {
                Ok(SettleOutcome::Applied)
            } else {
                Err(DispatchError::Rejected(
                    "credential realization receipt conflicts with committed evidence".to_string(),
                ))
            };
        }
        row.credential_receipts.push(receipt);
        Ok(SettleOutcome::Applied)
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
                    lease.expires_ms = crate::clock::deadline_millis(now_ms, lease_ms);
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
        let near_expiry = crate::clock::deadline_millis(now_ms, lease_ms / 2);
        for row in state.rows.values_mut() {
            if row.state == RowState::Leased
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.owner == owner && l.expires_ms < near_expiry)
            {
                if let Some(lease) = row.lease.as_mut() {
                    lease.expires_ms = crate::clock::deadline_millis(now_ms, lease_ms);
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
        let current = state
            .rows
            .get(run_id)
            .filter(|row| row.state == RowState::Leased && row.lease_epoch == epoch)
            .and_then(|row| row.lease.clone());
        let Some(lease) = current else {
            return Ok(SettleOutcome::Fenced);
        };
        let operation = DispatchOperation::Settled {
            claim: RunClaim::from(&lease),
            outcome,
        };
        match outcome {
            DispatchOutcome::Done => {
                let sequence = state.completions.len() as u64 + 1;
                state.completions.push(DispatchCompletion {
                    sequence,
                    run_id: run_id.clone(),
                });
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
        push_operation(&mut state, operation);
        Ok(SettleOutcome::Applied)
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .completions
            .iter()
            .filter(|completion| completion.sequence > after_sequence)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut operations = Vec::new();
        for row in state.rows.values_mut() {
            let expired = row.state == RowState::Leased
                && row.lease.as_ref().is_some_and(|l| l.expires_ms < now_ms);
            if expired && row.attempt_count >= max_attempts {
                let lease = row
                    .lease
                    .clone()
                    .expect("an expired running row carries its lease");
                let claim = RunClaim::from(&lease);
                operations.push(DispatchOperation::LeaseLost {
                    claim: claim.clone(),
                    reason: LeaseLossReason::RetryExhausted,
                });
                operations.push(DispatchOperation::DeadLettered {
                    claim,
                    attempt_count: row.attempt_count,
                });
                row.state = RowState::DeadLetter;
                row.lease = None;
                row.dead_lettered_at = Some(now_ms);
            }
        }
        let reaped = operations.len() / 2;
        for operation in operations {
            push_operation(&mut state, operation);
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
                    cancellation_requested: row.cancellation_requested,
                    attempt_count: row.attempt_count,
                    sandbox_bound: row.sandbox.is_some(),
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
            Some(RowState::Pending | RowState::Awaiting | RowState::Leased)
        );
        if !cancellable {
            return Ok(None);
        }
        let (thread, lost) = {
            let row = state.rows.get_mut(run_id).expect("cancellable row exists");
            let thread = row.request.thread_id().clone();
            row.cancellation_requested = true;
            let lost = if row.state == RowState::Leased {
                // Revoke the in-flight authority immediately. The stale owner keeps
                // its local cancellation token, but every later commit/settle under
                // its old epoch is fenced while cancellation becomes claimable now.
                let lease = row.lease.take();
                row.state = RowState::Pending;
                row.lease_epoch += 1;
                lease
            } else {
                None
            };
            (thread, lost)
        };
        if let Some(lease) = lost {
            push_operation(
                &mut state,
                DispatchOperation::LeaseLost {
                    claim: RunClaim::from(&lease),
                    reason: LeaseLossReason::Cancelled,
                },
            );
        }
        Ok(Some(thread))
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .find(|run| {
                state.rows.get(*run).is_some_and(|row| {
                    row.state == RowState::Awaiting
                        && !row.cancellation_requested
                        && row.request.thread_id() == thread_id
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
impl DispatchOperationalFeed for MemoryDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError> {
        let state = lock(&self.state)?;
        let events = state
            .operations
            .iter()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(DispatchPage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl Inbox for MemoryDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        append_pending(&mut state, input)
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
        let input = normalize_pending_millis(input);
        if let Some(existing) = state
            .outbox
            .iter()
            .find(|i| i.message_id == input.message_id)
        {
            return if existing == &input {
                Ok(false)
            } else {
                Err(DispatchError::Rejected(format!(
                    "idempotency key `{}` was reused with another outbox payload",
                    input.message_id
                )))
            };
        }
        state.outbox.push(input);
        Ok(true)
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        let mut state = lock(&self.state)?;
        // Validate the whole in-memory transaction before moving anything: an
        // outbox retry may match an existing pending row exactly, but the same
        // id with another payload is corruption and must leave the outbox intact.
        for input in &state.outbox {
            if let Some(existing) = state
                .pending
                .iter()
                .find(|pending| pending.input.message_id == input.message_id)
                && existing.input != *input
            {
                return Err(DispatchError::Rejected(format!(
                    "idempotency key `{}` was reused with another relayed payload",
                    input.message_id
                )));
            }
        }
        let staged = std::mem::take(&mut state.outbox);
        let relayed = staged.len();
        for input in staged {
            // Idempotent target append: skip a message already pending.
            if !state
                .pending
                .iter()
                .any(|p| p.input.message_id == input.message_id)
            {
                append_pending(&mut state, input)?;
            }
        }
        Ok(relayed)
    }
}
