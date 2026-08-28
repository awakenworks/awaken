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
    can_claim_locally, policy_selects_requester, retry_exhaustion_eligible,
};
use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    AttemptCredentialBinding, CasOutcome, Claimed, CommitEpochGuard, ContinuationAdmission,
    CredentialRealizationReceipt, DispatchCompletion, DispatchError, DispatchOutcome,
    DispatchQueue, DispatchState, DispatchSummary, ExactClaimMode, Inbox, Lease, Outbox,
    PendingInput, PendingRecord, RunClaim, RunIdentityDecision, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, StoredRunIdentity, SubmitOptions, can_admit_attempt_credentials,
    classify_completed_session_run_reservation, classify_exact_claim_mode,
    classify_live_session_run_reservation, classify_session_run_reservation_activation,
    compile_attempt_credential_bindings, decide_run_identity, ensure_session_child_capacity,
    installed_worker_credential_capabilities, normalize_pending_millis, session_child_parent,
    session_child_thread, validate_executable_dispatch_admission, validate_outbox_continuation,
    validate_session_resume_activity_transition, validate_session_resume_evidence,
    validate_session_resume_target, validate_session_run_reservation_request,
    validate_session_run_reservation_resolution, verify_credential_realization_receipt,
};
use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
use awaken_run_ingress_contract::{
    CancelTransition, DispatchTransition, DispatchTransitionError, GuardedTransition, RunDispatch,
};

#[derive(Debug, Clone)]
struct Row {
    request: RunDispatch,
    state: DispatchState,
    /// Set before signalling a live attempt. It remains true across lease expiry
    /// and recovery until the worker commits Cancelled and settles Done.
    cancellation_requested: bool,
    lease: Option<Lease>,
    /// Absolute expiry of the caller-owned admission window. Present only while
    /// `state == Reserved`; a recovery claim carries its ordinary lease instead.
    reservation_deadline_ms: Option<u64>,
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

fn guarded_claim_row(
    state: &State,
    claim: &RunClaim,
    required_state: DispatchState,
) -> Option<(RunDispatch, u64, bool)> {
    state.rows.get(&claim.run_id).and_then(|row| {
        (row.state == required_state
            && row.lease_epoch == claim.epoch
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
                row.cancellation_requested,
            )
        })
    })
}

impl Row {
    fn transition(&self) -> DispatchTransition {
        DispatchTransition {
            state: self.state,
            lease_epoch: self.lease_epoch,
            cancellation_requested: self.cancellation_requested,
        }
    }

    fn apply_transition(&mut self, transition: DispatchTransition) {
        self.state = transition.state;
        self.lease_epoch = transition.lease_epoch;
        self.cancellation_requested = transition.cancellation_requested;
    }

    fn exact_claim_deadline(&self) -> Option<u64> {
        match self.state {
            DispatchState::Reserved => self.reservation_deadline_ms,
            DispatchState::ReservationLeased => self.lease.as_ref().map(|lease| lease.expires_ms),
            _ => None,
        }
    }
}

fn transition_error(error: DispatchTransitionError) -> DispatchError {
    match error {
        DispatchTransitionError::LeaseEpochExhausted => {
            DispatchError::Rejected("dispatch claim epoch exhausted".to_string())
        }
    }
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
            .filter(|(_, row)| row.state == DispatchState::DeadLetter && keep(row))
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

    async fn lock_claim_epoch_in_state(
        &self,
        claim: &RunClaim,
        required_state: DispatchState,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        let guard = self.authority.clone().lock_owned().await;
        let state = lock(&self.state)?;
        let guarded = guarded_claim_row(&state, claim, required_state);
        drop(state);
        Ok(
            guarded.map(|(request, expires_ms, cancellation_requested)| {
                CommitEpochGuard::new(guard, request, expires_ms, cancellation_requested)
            }),
        )
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

/// Whether another open Run on the candidate's Thread blocks this claim.
///
/// The candidate is deliberately excluded: an Awaiting Run must be able to wake
/// itself from its exact reply or cancellation. Cancellation only waits for an
/// executing peer, so historical stores containing more than one Awaiting row can
/// drain those rows one at a time instead of deadlocking forever.
fn thread_has_claim_blocking_peer(
    state: &State,
    candidate_run: &RunId,
    include_awaiting: bool,
) -> bool {
    let Some(candidate) = state.rows.get(candidate_run) else {
        return false;
    };
    state.rows.iter().any(|(run_id, peer)| {
        run_id != candidate_run
            && peer.request.thread_id() == candidate.request.thread_id()
            && (matches!(
                peer.state,
                DispatchState::ReservationLeased | DispatchState::Leased
            ) || (include_awaiting && peer.state == DispatchState::Awaiting))
    })
}

/// Pick the next runnable run, oldest-first within each priority band: reclaim an
/// expired lease (recovery), then wake an awaiting run with pending input, then a
/// fresh pending run. This is the claim policy the SQL stores must match.
fn select_where(
    state: &State,
    now_ms: u64,
    mut compatible: impl FnMut(&Row) -> bool,
) -> Option<RunId> {
    let now_ms = crate::clock::normalize_millis(now_ms);
    // Session admission repair is a distinct claim mode: the resulting Worker
    // lease may call only the root activity authority until it is resolved.
    // Both reservation phases use the same deadline classifier as exact claims.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
                == ExactClaimMode::ReservationRecovery
            && !thread_has_claim_blocking_peer(state, run, false)
        {
            return Some(run.clone());
        }
    }

    // Cancellation is terminal control, not ordinary work. Once its owning lease
    // is free, claim it before wakes/fresh runs. Awaiting peers do not block this
    // branch, which lets a legacy multi-Awaiting Thread be cancelled sequentially.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.cancellation_requested
            && matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
            && !thread_has_claim_blocking_peer(state, run, false)
        {
            return Some(run.clone());
        }
    }

    // Recovery: re-own an expired-lease running row (first-match in enqueue order).
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == DispatchState::Leased
            && row.lease.as_ref().is_some_and(|l| l.expires_ms < now_ms)
            && compatible(row)
        {
            return Some(run.clone());
        }
    }
    // Wake: an awaiting run with due input whose Thread has no other open Run.
    // Excluding this exact row is what lets its reply wake it without opening a
    // fresh peer execution.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == DispatchState::Awaiting
            && state
                .pending
                .iter()
                .any(|p| &p.input.run_id == run && is_due(&p.input, now_ms))
            && !thread_has_claim_blocking_peer(state, run, true)
            && compatible(row)
        {
            return Some(run.clone());
        }
    }
    // Fresh work is ordered by priority (highest first), then enqueue order, and
    // only for Threads that have neither a Running nor Awaiting peer.
    let mut best: Option<(&RunId, i64)> = None;
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.state == DispatchState::Pending
            && !thread_has_claim_blocking_peer(state, run, true)
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
    if classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
        == ExactClaimMode::ReservationRecovery
        && !thread_has_claim_blocking_peer(state, run_id, false)
    {
        return Some(false);
    }
    if row.state == DispatchState::Leased
        && row
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_ms < now_ms)
    {
        return Some(true);
    }
    let thread_busy = thread_has_claim_blocking_peer(state, run_id, !row.cancellation_requested);
    if thread_busy {
        return None;
    }
    if row.state == DispatchState::Awaiting
        && (row.cancellation_requested
            || state
                .pending
                .iter()
                .any(|pending| pending.input.run_id == *run_id && is_due(&pending.input, now_ms)))
    {
        return Some(false);
    }
    (row.state == DispatchState::Pending).then_some(false)
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
    let mode = state
        .rows
        .get(requested_run)
        .map_or(ExactClaimMode::Runnable, |row| {
            classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
        });
    claim_exact_with_mode(
        state,
        requested_run,
        owner,
        lease_ms,
        now_ms,
        worker,
        capabilities,
        mode,
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
    let (was_execution_recovery, previous_lease) = match mode {
        ExactClaimMode::Runnable => {
            let Some(was_recovery) = runnable(state, requested_run, now_ms) else {
                return Ok(None);
            };
            let previous = was_recovery.then(|| {
                state
                    .rows
                    .get(requested_run)
                    .and_then(|row| row.lease.clone())
                    .expect("a recovery has an expired lease")
            });
            (was_recovery, previous)
        }
        ExactClaimMode::ReservationRecovery => {
            let Some(row) = state.rows.get(requested_run) else {
                return Ok(None);
            };
            let expired_recovery = row.state == DispatchState::ReservationLeased;
            if classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
                != ExactClaimMode::ReservationRecovery
                || thread_has_claim_blocking_peer(state, requested_run, false)
            {
                return Ok(None);
            }
            (
                false,
                expired_recovery.then(|| row.lease.clone().expect("leased repair")),
            )
        }
        ExactClaimMode::TerminalRecovery => {
            let Some(row) = state.rows.get(requested_run) else {
                return Ok(None);
            };
            let expired_running = row.state == DispatchState::Leased
                && row
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_ms < now_ms);
            let thread_busy = thread_has_claim_blocking_peer(state, requested_run, false);
            let quiescent_awaiting =
                row.state == DispatchState::Awaiting && row.lease.is_none() && !thread_busy;
            if !quiescent_awaiting && !expired_running {
                return Ok(None);
            }
            let previous = expired_running
                .then(|| row.lease.clone().expect("a recovery has an expired lease"));
            (expired_running, previous)
        }
        ExactClaimMode::RetryExhausted { max_attempts } => {
            let Some(row) = state.rows.get(requested_run) else {
                return Ok(None);
            };
            if !retry_exhaustion_eligible(
                row.state,
                row.lease.as_ref().map(|lease| lease.expires_ms),
                row.attempt_count,
                max_attempts,
                now_ms,
            ) {
                return Ok(None);
            }
            (
                true,
                Some(row.lease.clone().expect("retry-exhausted row has a lease")),
            )
        }
    };
    let terminal_resolution = mode.bypasses_execution_admission();
    let run_id = requested_run.clone();
    let row = state.rows.get(&run_id).expect("claimable row exists");
    if !terminal_resolution
        && worker.is_none()
        && !row.cancellation_requested
        && !can_claim_locally(&row.request.placement)
    {
        return Ok(None);
    }
    let claim_transition = if mode == ExactClaimMode::ReservationRecovery {
        row.transition().recover_reservation()
    } else {
        row.transition().claim()
    }
    .map_err(transition_error)?;
    let Some(claim_transition) = claim_transition else {
        return Ok(None);
    };
    let claim_epoch = claim_transition.lease_epoch;
    let credential_bindings = if terminal_resolution || row.cancellation_requested {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&row.request, capabilities, claim_epoch, now_ms)
            .map_err(|error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            })?
    };
    let assignment = (!terminal_resolution)
        .then(|| worker.map(WorkerAssignment::from))
        .flatten();
    let (request, sandbox, cancellation_requested, lease) = {
        let row = state.rows.get_mut(&run_id).expect("runnable row exists");
        row.apply_transition(claim_transition);
        let lease = Lease {
            run_id: run_id.clone(),
            owner: owner.to_string(),
            expires_ms: crate::clock::deadline_millis(now_ms, lease_ms),
            epoch: row.lease_epoch,
        };
        row.lease = Some(lease.clone());
        row.assignment = assignment.clone();
        row.credential_bindings.clone_from(&credential_bindings);
        row.credential_receipts.clear();
        if was_execution_recovery {
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
        recovered: was_execution_recovery,
        session_activity_admission_required: mode == ExactClaimMode::ReservationRecovery,
        sandbox,
        assignment,
    };
    if let Some(previous) = previous_lease {
        let previous = RunClaim::from(&previous);
        let claim = RunClaim::from(&lease);
        let reason = if matches!(mode, ExactClaimMode::RetryExhausted { .. }) {
            LeaseLossReason::RetryExhausted
        } else {
            LeaseLossReason::Expired
        };
        push_operation(
            state,
            DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason,
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
    let run_id = request.run_id().clone();
    let known = known_run_identity(state, &request)?;
    if !known {
        if !can_claim_locally(&request.placement) {
            return Ok(None);
        }
        enqueue_with_local(state, request, SubmitOptions::default())?;
    }
    claim_exact(state, &run_id, owner, lease_ms, now_ms, None, capabilities)
}

/// Same-Run replay is permitted only for the exact canonical dispatch. This is
/// evaluated by the queue authority before any supersession or claim mutation.
fn known_run_identity(state: &State, request: &RunDispatch) -> Result<bool, DispatchError> {
    let run_id = request.run_id();
    let stored = if let Some(row) = state.rows.get(run_id) {
        StoredRunIdentity::Live(&row.request)
    } else if let Some(completion) = state
        .completions
        .iter()
        .find(|completion| &completion.run_id == run_id)
    {
        StoredRunIdentity::Completed(completion.request_fingerprint.as_deref())
    } else {
        StoredRunIdentity::Absent
    };
    decide_run_identity(stored, request).map(|decision| decision == RunIdentityDecision::Replay)
}

/// The ordinary in-memory enqueue kernel. Exact replay is classified before
/// executable admission so an existing reservation remains recoverable; only a
/// genuinely new row reaches the ordinary-shape guard and insertion kernel.
fn enqueue_with_local(
    state: &mut State,
    request: RunDispatch,
    options: SubmitOptions,
) -> Result<(), DispatchError> {
    if known_run_identity(state, &request)? {
        return Ok(());
    }
    validate_executable_dispatch_admission(&request)?;
    enqueue_new_local(state, request, options)
}

/// Insert one identity-checked row. Session reservation is the only caller
/// allowed to bypass ordinary executable admission, and it reaches this helper
/// only after its dedicated shape and replay checks have succeeded.
fn enqueue_new_local(
    state: &mut State,
    request: RunDispatch,
    options: SubmitOptions,
) -> Result<(), DispatchError> {
    let run_id = request.run_id().clone();
    if let Some(key) = &options.dedupe_key
        && state.rows.values().any(|row| {
            row.dedupe_key.as_deref() == Some(key) && row.state != DispatchState::DeadLetter
        })
    {
        return Ok(());
    }

    let thread = request.thread_id().clone();
    let mut epoch = 0;
    if options.supersede {
        let max_epoch = state
            .rows
            .values()
            .filter(|row| *row.request.thread_id() == thread)
            .map(|row| row.epoch)
            .max()
            .unwrap_or(0);
        epoch = crate::next_supersession_epoch(max_epoch)?;
        for row in state.rows.values_mut() {
            if *row.request.thread_id() == thread
                && matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
                && !row.cancellation_requested
            {
                row.state = DispatchState::Superseded;
                row.lease = None;
            }
        }
    }
    state.rows.insert(
        run_id.clone(),
        Row {
            request,
            state: DispatchState::Pending,
            cancellation_requested: false,
            lease: None,
            reservation_deadline_ms: None,
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

fn known_session_child_threads(state: &State, parent: &ThreadId) -> Vec<ThreadId> {
    state
        .rows
        .values()
        .filter_map(|row| session_child_thread(&row.request, parent).cloned())
        .chain(state.completions.iter().filter_map(|completion| {
            (completion.session_thread_id.as_ref() == Some(parent))
                .then(|| completion.thread_id.clone())
                .flatten()
                .filter(|thread| thread != parent)
        }))
        .collect()
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
    ) -> Result<Option<RunClaim>, DispatchError> {
        let owner = identity.lease_owner();
        let state = lock(&self.state)?;
        Ok(state.rows.get(run_id).and_then(|row| {
            (row.state == DispatchState::Leased && !row.cancellation_requested)
                .then_some(row.lease.as_ref())
                .flatten()
                .filter(|lease| lease.owner == owner && lease.expires_ms >= now_ms)
                .map(|_| RunClaim {
                    run_id: run_id.clone(),
                    owner,
                    epoch: row.lease_epoch,
                })
        }))
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_state(claim, DispatchState::Leased)
            .await
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_state(claim, DispatchState::ReservationLeased)
            .await
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_deadline_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let reservation_deadline_ms =
            validate_session_run_reservation_request(&request, reservation_deadline_ms)?;
        let run_id = request.run_id().clone();
        if let Some(row) = state.rows.get(&run_id) {
            return Ok(classify_live_session_run_reservation(
                &row.request,
                row.state,
                &request,
            ));
        }
        if let Some(completion) = state
            .completions
            .iter()
            .find(|completion| completion.run_id == run_id)
        {
            return Ok(classify_completed_session_run_reservation(
                completion.request_fingerprint.as_deref(),
                &request,
            ));
        }
        enqueue_new_local(&mut state, request, SubmitOptions::default())?;
        let row = state
            .rows
            .get_mut(&run_id)
            .expect("newly reserved dispatch exists");
        row.state = DispatchState::Reserved;
        row.reservation_deadline_ms = Some(reservation_deadline_ms);
        Ok(SessionRunReservationOutcome::Reserved)
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let completed = state
            .completions
            .iter()
            .any(|completion| &completion.run_id == run_id);
        let live = state.rows.get(run_id).map(|row| (&row.request, row.state));
        if let Some(outcome) = classify_session_run_reservation_activation(
            live,
            completed,
            session_thread_id,
            session_activity_epoch,
        )? {
            return Ok(outcome);
        }
        let row = state
            .rows
            .get_mut(run_id)
            .expect("classified live reservation");
        let next = row
            .transition()
            .activate_reservation()
            .expect("classifier admitted only Reserved");
        row.request.session_activity_epoch = Some(session_activity_epoch);
        row.apply_transition(next);
        row.reservation_deadline_ms = None;
        Ok(SessionRunReservationActivation::Activated)
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        if state
            .rows
            .get(run_id)
            .is_none_or(|row| row.state != DispatchState::Reserved || row.lease.is_some())
        {
            return Ok(false);
        }
        state.rows.remove(run_id);
        state.order.retain(|candidate| candidate != run_id);
        state
            .pending
            .retain(|pending| &pending.input.run_id != run_id);
        Ok(true)
    }

    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get(&claim.run_id) else {
            return Ok(SettleOutcome::Fenced);
        };
        let owner_matches = row
            .lease
            .as_ref()
            .is_some_and(|lease| lease.owner == claim.owner);
        let transition_resolution = match resolution {
            SessionRunReservationResolution::Admitted { .. } => Some(true),
            SessionRunReservationResolution::Retry { .. } => Some(false),
            SessionRunReservationResolution::Rejected => None,
        };
        let transition =
            row.transition()
                .resolve_reservation(claim.epoch, owner_matches, transition_resolution);
        if transition == GuardedTransition::Fenced {
            return Ok(SettleOutcome::Fenced);
        }
        match transition {
            GuardedTransition::Removed => {
                state.rows.remove(&claim.run_id);
                state.order.retain(|candidate| candidate != &claim.run_id);
                state
                    .pending
                    .retain(|pending| pending.input.run_id != claim.run_id);
            }
            GuardedTransition::Applied(next) => {
                let reorder = matches!(resolution, SessionRunReservationResolution::Retry { .. });
                {
                    let row = state
                        .rows
                        .get_mut(&claim.run_id)
                        .expect("resolved reservation exists");
                    match resolution {
                        SessionRunReservationResolution::Admitted {
                            session_activity_epoch,
                        } => {
                            row.request.session_activity_epoch = Some(session_activity_epoch);
                            row.reservation_deadline_ms = None;
                            row.lease = None;
                            row.assignment = None;
                            row.credential_bindings.clear();
                            row.credential_receipts.clear();
                        }
                        SessionRunReservationResolution::Retry {
                            reservation_deadline_ms,
                        } => {
                            row.lease = None;
                            row.assignment = None;
                            row.credential_bindings.clear();
                            row.credential_receipts.clear();
                            row.reservation_deadline_ms = Some(reservation_deadline_ms);
                        }
                        SessionRunReservationResolution::Rejected => unreachable!(),
                    }
                    row.apply_transition(next);
                }
                if reorder {
                    state.order.retain(|candidate| candidate != &claim.run_id);
                    state.order.push(claim.run_id.clone());
                }
            }
            GuardedTransition::Fenced => unreachable!(),
        }
        Ok(SettleOutcome::Applied)
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        enqueue_with_local(&mut state, request, options)
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        // Identity has precedence over mutable capacity: exact at-least-once
        // retries succeed at the cap, while same-id payload collisions fail
        // before any sibling is inspected or changed.
        if known_run_identity(&state, &request)? {
            return Ok(());
        }
        let parent = session_child_parent(&request)?.clone();
        ensure_session_child_capacity(
            &request,
            &admission,
            known_session_child_threads(&state, &parent),
        )?;
        enqueue_with_local(&mut state, request, SubmitOptions::default())
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
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        let known = known_run_identity(&state, &request)?;
        if !known {
            if can_assign(worker, &request.placement, None, false, now_ms).is_err() {
                return Ok(None);
            }
            enqueue_with_local(&mut state, request, SubmitOptions::default())?;
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
                    recovered: row.state == DispatchState::Leased,
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

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let now_ms = crate::clock::normalize_millis(now_ms);
        let Some(run_id) = state.order.iter().find_map(|run_id| {
            state.rows.get(run_id).and_then(|row| {
                retry_exhaustion_eligible(
                    row.state,
                    row.lease.as_ref().map(|lease| lease.expires_ms),
                    row.attempt_count,
                    max_attempts,
                    now_ms,
                )
                .then(|| run_id.clone())
            })
        }) else {
            return Ok(None);
        };
        claim_exact_with_mode(
            &mut state,
            &run_id,
            owner,
            lease_ms,
            now_ms,
            None,
            &Default::default(),
            ExactClaimMode::RetryExhausted { max_attempts },
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
        if row.state != DispatchState::Leased
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
            && row.state == DispatchState::Leased
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
                if matches!(
                    row.state,
                    DispatchState::ReservationLeased | DispatchState::Leased
                ) && row.lease.as_ref().is_some_and(|l| l.owner == owner) =>
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
            if matches!(
                row.state,
                DispatchState::ReservationLeased | DispatchState::Leased
            ) && row
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

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let transition = state
            .rows
            .get(&claim.run_id)
            .map_or(GuardedTransition::Fenced, |row| {
                row.transition().relinquish(
                    claim.epoch,
                    row.lease
                        .as_ref()
                        .is_some_and(|lease| lease.owner == claim.owner),
                )
            });
        let GuardedTransition::Applied(next) = transition else {
            return Ok(SettleOutcome::Fenced);
        };
        if let Some(row) = state.rows.get_mut(&claim.run_id) {
            row.apply_transition(next);
            row.lease = None;
        }
        // Re-enter at the tail of its priority cohort. Otherwise one
        // temporarily unresolvable head item would be claimed again before a
        // newly submitted runnable peer and starve the queue.
        state.order.retain(|run_id| run_id != &claim.run_id);
        state.order.push(claim.run_id.clone());
        push_operation(
            &mut state,
            DispatchOperation::LeaseLost {
                claim: claim.clone(),
                reason: LeaseLossReason::Relinquished,
            },
        );
        Ok(SettleOutcome::Applied)
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
        let Some((transition, lease)) = state.rows.get(run_id).and_then(|row| {
            let transition = row
                .transition()
                .settle(epoch, outcome == DispatchOutcome::Done);
            row.lease.clone().map(|lease| (transition, lease))
        }) else {
            return Ok(SettleOutcome::Fenced);
        };
        if transition == GuardedTransition::Fenced {
            return Ok(SettleOutcome::Fenced);
        }
        let operation = DispatchOperation::Settled {
            claim: RunClaim::from(&lease),
            outcome,
        };
        match outcome {
            DispatchOutcome::Done => {
                debug_assert_eq!(transition, GuardedTransition::Removed);
                let sequence = state.completions.len() as u64 + 1;
                let request = state.rows.get(run_id).map(|row| {
                    (
                        row.request.thread_id().clone(),
                        row.request.session_thread_id.clone(),
                        row.request.canonical_fingerprint(),
                    )
                });
                state.completions.push(DispatchCompletion {
                    sequence,
                    run_id: run_id.clone(),
                    thread_id: request.as_ref().map(|(thread, _, _)| thread.clone()),
                    session_thread_id: request.as_ref().and_then(|(_, parent, _)| parent.clone()),
                    request_fingerprint: request.map(|(_, _, fingerprint)| fingerprint),
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
                let GuardedTransition::Applied(next) = transition else {
                    unreachable!("the transition kernel maps exact awaiting settlement")
                };
                if let Some(row) = state.rows.get_mut(run_id) {
                    row.apply_transition(next);
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

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut operations = Vec::new();
        for row in state.rows.values_mut() {
            let expired = row.state == DispatchState::Leased
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
                row.state = DispatchState::DeadLetter;
                row.lease = None;
                row.dead_lettered_at = Some(now_ms);
            }
        }
        let quarantined = operations.len() / 2;
        for operation in operations {
            push_operation(&mut state, operation);
        }
        Ok(quarantined)
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .filter(|run| {
                matches!(
                    state.rows.get(run).map(|r| r.state),
                    Some(DispatchState::DeadLetter)
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
                    Some(DispatchState::Superseded)
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
                    session_thread_id: row.request.session_thread_id.clone(),
                    session_activity_epoch: row.request.session_activity_epoch,
                    reservation_deadline_ms: (row.state == DispatchState::Reserved)
                        .then_some(row.reservation_deadline_ms)
                        .flatten(),
                    state: row.state,
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
            Some(row) if row.state == DispatchState::DeadLetter && !row.cancellation_requested => {
                row.state = DispatchState::Pending;
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
        let Some(row) = state.rows.get(run_id) else {
            return Ok(None);
        };
        let was_reservation_claim = row.state == DispatchState::ReservationLeased;
        let transition = row.transition().cancel().map_err(transition_error)?;
        let CancelTransition::Applied {
            state: next,
            revoked_lease,
        } = transition
        else {
            return Ok(None);
        };
        let (thread, lost) = {
            let row = state.rows.get_mut(run_id).expect("cancellable row exists");
            let thread = row.request.thread_id().clone();
            let lost = if revoked_lease {
                // Revoke the in-flight authority immediately. The stale owner keeps
                // its local cancellation token, but every later commit/settle under
                // its old epoch is fenced while cancellation becomes claimable now.
                row.lease.take()
            } else {
                None
            };
            row.apply_transition(next);
            if was_reservation_claim && row.state == DispatchState::Reserved {
                row.reservation_deadline_ms = Some(0);
            }
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
                    row.state == DispatchState::Awaiting
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

    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: u64,
    ) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        let input = normalize_pending_millis(input);
        let row = state.rows.get(&input.run_id).ok_or_else(|| {
            DispatchError::Rejected(format!(
                "Session resume Run `{}` was not found",
                input.run_id.0
            ))
        })?;
        validate_session_resume_target(
            &row.request,
            &input,
            session_thread_id,
            prior_session_activity_epoch,
            session_activity_epoch,
        )?;
        let current_epoch = row.request.session_activity_epoch;
        let accepts_new_resume = !row.cancellation_requested
            && matches!(
                (row.state, row.lease.is_some()),
                (DispatchState::Pending | DispatchState::Awaiting, false)
                    | (DispatchState::Leased, true)
            );
        let exact = validate_session_resume_evidence(
            &input,
            state
                .outbox
                .iter()
                .chain(state.pending.iter().map(|pending| &pending.input)),
        )?;
        if !validate_session_resume_activity_transition(
            current_epoch,
            prior_session_activity_epoch,
            session_activity_epoch,
            exact,
        )? {
            return Ok(false);
        }
        if !accepts_new_resume {
            return Err(DispatchError::Rejected(
                "Session resume requires a Pending, Awaiting, or currently Leased dispatch"
                    .to_string(),
            ));
        }
        state
            .rows
            .get_mut(&input.run_id)
            .expect("validated dispatch row remains under the transaction mutex")
            .request
            .session_activity_epoch = Some(session_activity_epoch);
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

    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        let message_id = input.message_id.clone();
        // Canonical Run identity is checked first so a conflicting retry cannot
        // consume or alter the independently durable report.
        let replay = known_run_identity(&state, &request)?;
        let position = state
            .outbox
            .iter()
            .position(|candidate| candidate.message_id == message_id);
        let existing = position
            .map(|position| state.outbox[position].clone())
            .or_else(|| {
                state
                    .pending
                    .iter()
                    .find(|pending| pending.input.message_id == message_id)
                    .map(|pending| pending.input.clone())
            });
        if existing.as_ref().is_some_and(|existing| existing != &input) {
            return Err(DispatchError::Rejected(format!(
                "idempotency key `{message_id}` was reused with another continuation payload"
            )));
        }
        validate_outbox_continuation(&input, &request, &admission)?;
        if replay {
            if let Some(position) = position {
                state.outbox.remove(position);
            }
            return Ok(());
        }

        // The in-memory mutex is the transaction boundary. Validate pending
        // idempotency before enqueue so every remaining step is infallible and
        // an error cannot expose a half-applied state.
        if let Some(existing) = state
            .pending
            .iter()
            .find(|pending| pending.input.message_id == input.message_id)
            && existing.input != input
        {
            return Err(DispatchError::Rejected(format!(
                "idempotency key `{}` was reused with another pending-input payload",
                input.message_id
            )));
        }
        if let ContinuationAdmission::SessionChild(policy) = &admission {
            let parent = session_child_parent(&request)?.clone();
            ensure_session_child_capacity(
                &request,
                policy,
                known_session_child_threads(&state, &parent),
            )?;
        }
        enqueue_with_local(&mut state, request, SubmitOptions::default())?;
        append_pending(&mut state, input)?;
        if let Some(position) = position {
            state.outbox.remove(position);
        }
        Ok(())
    }
}
