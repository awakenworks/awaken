// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint ingress.epoch_reclaim
// @formal entrypoint ingress.pending_input_exactly_once
// @formal entrypoint ingress.settle_fence
// @formal entrypoint ingress.single_owner
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal entrypoint run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
// @formal entrypoint worker.retry_preserves_pinned_binding
// @formal evidence run_ingress.a_deadline_passes_once_it_is_reached
// awaken-allow(file-limit-ceiling): 1739
//! In-memory reference implementation of the durable-ingress store.
//!
//! It mirrors the Postgres store's behaviour exactly so the worker and ingress
//! can be tested without a database (the in-memory commit coordinator
//! plays for the commit boundary). It is the executable specification of the
//! claim/lease/wake/recovery rules; the Postgres store must match it.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Arc;

use awaken_sync::InvariantLock;

use crate::{
    DispatchPlacement, PlacementPolicy, WorkerAssignment, WorkerSnapshot, can_assign,
    can_claim_locally, policy_selects_requester, retry_exhaustion_eligible,
};
use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, DispatchPage, LeaseLossReason,
};
use crate::{
    dispatch::AttemptAdmission, dispatch::AttemptExecutionBinding, dispatch::CasOutcome,
    dispatch::ClaimCredentialOperation, dispatch::Claimed, dispatch::CommitEpochGuard,
    dispatch::ContinuationAdmission, dispatch::DispatchCompletion, dispatch::DispatchDepths,
    dispatch::DispatchError, dispatch::DispatchLease, dispatch::DispatchOutcome,
    dispatch::DispatchState, dispatch::DispatchSummary, dispatch::ExactClaimMode, dispatch::Inbox,
    dispatch::LeaseRenewalOutcome, dispatch::Outbox, dispatch::PendingInput,
    dispatch::PendingInputSettlementPlan, dispatch::PendingRecord, dispatch::PhysicalAttempt,
    dispatch::RunIdentityDecision, dispatch::SessionChildAdmission,
    dispatch::SessionRunReservationActivation, dispatch::SessionRunReservationOutcome,
    dispatch::SessionRunReservationResolution, dispatch::SettleOutcome,
    dispatch::StoredRunIdentity, dispatch::SubmitOptions, dispatch::can_admit_claim_credentials,
    dispatch::claim_credential_operation, dispatch::claim_with_frozen,
    dispatch::classify_completed_session_run_reservation, dispatch::classify_exact_claim_mode,
    dispatch::classify_live_session_run_reservation,
    dispatch::classify_session_run_reservation_activation,
    dispatch::compile_claim_credential_bindings, dispatch::decide_lease_renewal,
    dispatch::decide_run_identity, dispatch::ensure_session_child_capacity,
    dispatch::installed_worker_credential_capabilities, dispatch::normalize_pending_millis,
    dispatch::pending_input_settlement_plan, dispatch::session_child_parent,
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
    CancelTransition, CommitEpochAdmission, DispatchTransition, GuardedTransition,
    PhysicalAttemptSlot, PhysicalAttemptTransition, ReleasedAttempt, RelinquishTransition,
    RunDispatch, classify_commit_epoch_admission,
};
use awaken_worker_contract::RunClaim;

mod credential_binding;
mod inbox;
mod outbox;
mod physical_attempt;
mod selection;
mod settlement;
mod store;
use awaken_time::WallClockExt;
use selection::{retry_deadline_is_due, runnable, select_where, thread_has_claim_blocking_peer};
pub use store::MemoryDispatchStore;

#[derive(Debug, Clone)]
struct Row {
    request: RunDispatch,
    state: DispatchState,
    /// Set before signalling a live attempt. It remains true across lease expiry
    /// and recovery until the worker commits Cancelled and settles Done.
    cancellation_requested: bool,
    lease: Option<DispatchLease>,
    /// Absolute expiry of the caller-owned admission window. Present only while
    /// `state == Reserved`; a recovery claim carries its ordinary lease instead.
    reservation_deadline_ms: Option<u64>,
    /// Consecutive crash-recoveries without a settle; reset when the run awaits.
    attempt_count: u64,
    /// Request-local resolution backoff. Cancellation clears the deadline before
    /// its next claim; terminal repair uses its own exact authority.
    retry_not_before_ms: Option<u64>,
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
    /// Exact physical executor currently inside model/tool/Sandbox work. This is
    /// part of the Dispatch claim aggregate and deliberately survives a lease
    /// reclaim until that predecessor acknowledges quiescence.
    active_attempt: Option<RunClaim>,
    active_attempt_binding: Option<AttemptExecutionBinding>,
    retired_attempt_epoch: u64,
    active_attempt_closing: bool,
}

fn guarded_claim_row(
    state: &State,
    claim: &RunClaim,
    required_state: DispatchState,
) -> Result<Option<(RunDispatch, u64, bool, PhysicalAttemptSlot)>, DispatchError> {
    let Some(row) = state.rows.get(&claim.run_id) else {
        return Ok(None);
    };
    let lease = row.lease.as_ref();
    match classify_commit_epoch_admission(
        row.state == required_state,
        lease.is_some(),
        lease.is_some(),
        lease.is_some_and(|lease| lease.owner() == claim.owner),
        row.lease_epoch == claim.epoch,
    ) {
        CommitEpochAdmission::Fenced => Ok(None),
        CommitEpochAdmission::Corrupt => Err(DispatchError::Rejected(
            "current dispatch claim has incomplete lease authority".to_string(),
        )),
        CommitEpochAdmission::Guarded => {
            let lease = lease.expect("guard admission requires a complete lease");
            Ok(Some((
                row.request.clone(),
                lease.expires_ms(),
                row.cancellation_requested,
                PhysicalAttemptSlot::from_facts(
                    row.active_attempt.is_some(),
                    row.active_attempt.is_some(),
                    row.active_attempt.as_ref() == Some(claim),
                    row.active_attempt.as_ref() == Some(claim),
                ),
            )))
        }
    }
}

impl Row {
    fn summary(&self, run_id: &RunId) -> Result<DispatchSummary, DispatchError> {
        Ok(DispatchSummary {
            run_id: run_id.clone(),
            thread_id: self.request.thread_id().clone(),
            session_thread_id: self.request.session_thread_id.clone(),
            session_activity_epoch: self.request.session_activity_epoch,
            reservation_deadline_ms: (self.state == DispatchState::Reserved)
                .then_some(self.reservation_deadline_ms)
                .flatten(),
            state: self.state,
            cancellation_requested: self.cancellation_requested,
            attempt_count: self.attempt_count,
            sandbox_bound: self.sandbox.is_some(),
            physical_attempt_active: self.physical_attempt()?.is_some(),
        })
    }

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
            DispatchState::ReservationLeased => self.lease.as_ref().map(|lease| lease.expires_ms()),
            _ => None,
        }
    }
}

/// A pending input with its optimistic-concurrency revision.
#[derive(Debug, Clone)]
struct PendingRow {
    input: PendingInput,
    revision: u64,
}

/// One record in the original completion log; the operational receipt is
/// unchanged while eligible Task sources stay in this same authority record.
struct CompletionRecord {
    receipt: DispatchCompletion,
    retained_request: Option<Box<RunDispatch>>,
}

struct State {
    /// The store's clock, reachable under the lock so every operational event
    /// is stamped on the same timeline the store judges its rows on.
    clock: Arc<dyn awaken_time::WallClock>,
    /// Enqueue order, so claim is deterministic (oldest first).
    order: Vec<RunId>,
    rows: BTreeMap<RunId, Row>,
    /// Undelivered pending input, in arrival order.
    pending: Vec<PendingRow>,
    /// Cross-thread deliveries staged for relay, in arrival order.
    outbox: Vec<PendingInput>,
    /// Applied-Done facts, retained as permanent run-id tombstones (ADR-0060).
    completions: Vec<CompletionRecord>,
    /// Dispatch-authority transitions in the same order as their mutations.
    operations: Vec<crate::dispatch::StoredDispatchEvent>,
}

impl State {
    fn new(clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self {
            clock,
            order: Vec::new(),
            rows: BTreeMap::new(),
            pending: Vec::new(),
            outbox: Vec::new(),
            completions: Vec::new(),
            operations: Vec::new(),
        }
    }
}

impl MemoryDispatchStore {
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
            .filter(|(_, row)| {
                // GC shares the canonical empty-slot rule. A legacy unbound
                // executor, orphan binding or closing residue retains this
                // exact row; deleting it would manufacture physical absence.
                row.state == DispatchState::DeadLetter
                    && keep(row)
                    && crate::dispatch::observed_physical_attempt_is_present(
                        row.active_attempt.is_some(),
                        row.active_attempt.is_some(),
                        row.active_attempt_binding.is_some(),
                        row.active_attempt_closing,
                    )
                    .is_ok_and(|present| !present)
            })
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
        let guarded = guarded_claim_row(&state, claim, required_state)?;
        drop(state);
        Ok(guarded.map(
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

fn lock(state: &InvariantLock<State>) -> Result<std::sync::MutexGuard<'_, State>, DispatchError> {
    state
        .lock()
        .map_err(|_| DispatchError::unavailable("dispatch store poisoned"))
}

fn push_operation(
    state: &mut State,
    operation: impl Into<crate::dispatch::StoredDispatchOperation>,
) {
    state.operations.push(crate::dispatch::StoredDispatchEvent {
        cursor: DispatchCursor(state.operations.len() as u64 + 1),
        recorded_at_ms: Some(awaken_time::WallClockExt::now_ms(&*state.clock)),
        operation: operation.into(),
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
            Err(DispatchError::Conflict(format!(
                "idempotency key `{}` was reused with another pending-input payload",
                input.message_id
            )))
        };
    }
    state.pending.push(PendingRow { input, revision: 1 });
    Ok(true)
}

fn claim_exact(
    state: &mut State,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
            if !retry_deadline_is_due(row, now_ms)
                || classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
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
                && crate::clock::deadline_has_passed(
                    row.lease.as_ref().map(|lease| lease.expires_ms()),
                    now_ms,
                );
            let thread_busy = thread_has_claim_blocking_peer(state, requested_run, false);
            let quiescent_unleased =
                matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
                    && row.lease.is_none()
                    && !thread_busy;
            if !retry_deadline_is_due(row, now_ms)
                || row.active_attempt.is_some()
                || (!quiescent_unleased && !expired_running)
            {
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
                row.lease.as_ref().map(|lease| lease.expires_ms()),
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
    if row
        .request
        .worker_codec_admission(worker.map(|worker| &worker.manifest))
        .is_err()
    {
        return Ok(None);
    }
    if !terminal_resolution
        && !row.cancellation_requested
        && match worker {
            Some(worker) => can_assign(
                worker,
                &row.request.placement,
                row.assignment.as_ref(),
                row.sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&row.request.placement),
        }
    {
        return Ok(None);
    }
    let claimed = claim_with_frozen(row.transition(), row.request.clone(), mode)
        .map_err(crate::transition_error)?;
    let Some((claim_transition, request)) = claimed else {
        return Ok(None);
    };
    let credential_operation =
        claim_credential_operation(terminal_resolution, row.cancellation_requested);
    let credential_bindings = compile_claim_credential_bindings(
        &request,
        capabilities,
        claim_transition.lease_epoch,
        now_ms,
        credential_operation,
    )
    .map_err(|error| {
        DispatchError::Rejected(format!("credential claim admission failed: {error}"))
    })?;
    let assignment = (!terminal_resolution)
        .then(|| worker.map(WorkerAssignment::from))
        .flatten();
    let (sandbox, cancellation_requested, lease) = {
        let row = state.rows.get_mut(&run_id).expect("runnable row exists");
        row.apply_transition(claim_transition);
        let lease = DispatchLease::from_persisted(
            run_id.clone(),
            owner.to_string(),
            crate::clock::deadline_millis(now_ms, lease_ms),
            claim_transition.lease_epoch,
        )?;
        row.lease = Some(lease.clone());
        row.assignment = assignment.clone();
        row.credential_bindings.clone_from(&credential_bindings);
        row.credential_receipts.clear();
        row.retry_not_before_ms = None;
        if was_execution_recovery {
            row.attempt_count += 1;
        }
        (row.sandbox.clone(), row.cancellation_requested, lease)
    };
    let pending = state
        .pending
        .iter()
        .filter(|pending| pending.input.run_id == run_id && is_due(&pending.input, now_ms))
        .map(|pending| pending.input.clone())
        .collect();
    let claimed = Claimed {
        session_work_authority: None,
        request,
        lease: lease.clone(),
        credential_bindings,
        credential_deliveries: Vec::new(),
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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

/// Borrow the original row-first evidence without cloning an admission body.
fn stored_run_evidence<'a>(
    state: &'a State,
    run_id: &RunId,
) -> (Option<&'a Row>, Option<&'a CompletionRecord>) {
    let live = state.rows.get(run_id);
    let completed = live
        .is_none()
        .then(|| {
            state
                .completions
                .iter()
                .find(|row| &row.receipt.run_id == run_id)
        })
        .flatten();
    (live, completed)
}

/// Same-Run replay is permitted only for the exact canonical dispatch. This is
/// evaluated by the queue authority before any supersession or claim mutation.
fn known_run_identity(state: &State, request: &RunDispatch) -> Result<bool, DispatchError> {
    let run_id = request.run_id();
    let (live, completed) = stored_run_evidence(state, run_id);
    let stored = match (live, completed) {
        (Some(row), _) => StoredRunIdentity::Live(&row.request),
        (None, Some(completion)) => {
            StoredRunIdentity::Completed(completion.receipt.request_fingerprint.as_deref())
        }
        (None, None) => StoredRunIdentity::Absent,
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
        validate_session_run_replacement_candidates(
            &request,
            state
                .rows
                .values()
                .filter(|row| *row.request.thread_id() == thread)
                .map(|row| row.state),
        )?;
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
                && let Some(next) = row.transition().supersede()
            {
                row.apply_transition(next);
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
            retry_not_before_ms: None,
            priority: options.priority,
            epoch,
            lease_epoch: 0,
            dedupe_key: options.dedupe_key,
            dead_lettered_at: None,
            sandbox: None,
            assignment: None,
            credential_bindings: Vec::new(),
            credential_receipts: Vec::new(),
            active_attempt: None,
            active_attempt_binding: None,
            retired_attempt_epoch: 0,
            active_attempt_closing: false,
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
            (completion.receipt.session_thread_id.as_ref() == Some(parent))
                .then(|| completion.receipt.thread_id.clone())
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let Some(run_id) = select_where(state, now_ms, None, |row| {
        (row.cancellation_requested || can_claim_locally(&row.request.placement))
            && row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                can_admit_claim_credentials(
                    &row.request,
                    capabilities,
                    epoch,
                    now_ms,
                    claim_credential_operation(false, row.cancellation_requested),
                )
            })
    }) else {
        return Ok(None);
    };
    claim_exact(state, &run_id, owner, lease_ms, now_ms, None, capabilities)
}

fn release_claim(
    state: &mut State,
    claim: &RunClaim,
    retry_not_before_ms: Option<u64>,
    attempt: ReleasedAttempt,
) -> Result<SettleOutcome, DispatchError> {
    let transition = state
        .rows
        .get(&claim.run_id)
        .map_or(RelinquishTransition::Fenced, |row| {
            let slot = match row.active_attempt.as_ref() {
                None => PhysicalAttemptSlot::Empty,
                Some(active) if active == claim => PhysicalAttemptSlot::ExactClaim,
                Some(_) => PhysicalAttemptSlot::OtherClaim,
            };
            row.transition().relinquish(
                claim.epoch,
                row.lease
                    .as_ref()
                    .is_some_and(|lease| lease.owner() == claim.owner),
                slot,
                row.active_attempt.as_ref().map(|active| active.epoch),
                row.attempt_count,
                attempt,
            )
        });
    match transition {
        RelinquishTransition::Pending {
            state: next,
            attempt_count,
        } => {
            if let Some(row) = state.rows.get_mut(&claim.run_id) {
                row.apply_transition(next);
                row.attempt_count = attempt_count;
                row.lease = None;
                row.retry_not_before_ms = retry_not_before_ms;
            }
        }
        RelinquishTransition::Recovering {
            state: next,
            attempt_count,
        } => {
            if let Some(row) = state.rows.get_mut(&claim.run_id) {
                row.apply_transition(next);
                row.attempt_count = attempt_count;
                let lease = row
                    .lease
                    .as_mut()
                    .expect("an exact leased claim has a lease");
                *lease = lease.with_deadline(0);
                row.retry_not_before_ms = retry_not_before_ms;
            }
        }
        RelinquishTransition::Fenced => return Ok(SettleOutcome::Fenced),
        RelinquishTransition::Corrupt => {
            return Err(DispatchError::Rejected(
                "persisted physical attempt slot is not an owner/epoch pair".to_string(),
            ));
        }
    }
    // Re-enter at the tail of its priority cohort. Otherwise one temporarily
    // unresolvable head item would be claimed again before a runnable peer.
    state.order.retain(|run_id| run_id != &claim.run_id);
    state.order.push(claim.run_id.clone());
    push_operation(
        state,
        DispatchOperation::LeaseLost {
            claim: claim.clone(),
            reason: LeaseLossReason::Relinquished,
        },
    );
    Ok(SettleOutcome::Applied)
}

#[async_trait]
impl awaken_dispatch_contract::DispatchCommitAuthority for MemoryDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let owner = identity.lease_owner();
        let state = lock(&self.state)?;
        Ok(state.rows.get(run_id).and_then(|row| {
            (row.state == DispatchState::Leased && !row.cancellation_requested)
                .then_some(row.lease.as_ref())
                .flatten()
                .filter(|lease| {
                    lease.owner() == owner
                        && !crate::clock::deadline_has_passed(Some(lease.expires_ms()), now_ms)
                })
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
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaims for MemoryDispatchStore {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let retry_deadline_ms = match resolution {
            SessionRunReservationResolution::Retry { reservation_ttl_ms } => Some(
                crate::clock::deadline_millis(self.clock.now_ms(), reservation_ttl_ms),
            ),
            _ => None,
        };
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get(&claim.run_id) else {
            return Ok(SettleOutcome::Fenced);
        };
        let owner_matches = row
            .lease
            .as_ref()
            .is_some_and(|lease| lease.owner() == claim.owner);
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
                        SessionRunReservationResolution::Retry { .. } => {
                            row.lease = None;
                            row.assignment = None;
                            row.credential_bindings.clear();
                            row.credential_receipts.clear();
                            row.reservation_deadline_ms = retry_deadline_ms;
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

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_new_local(&mut state, request, owner, lease_ms, now_ms, capabilities)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        deliver_and_claim_local(&mut state, input, owner, lease_ms, now_ms, capabilities)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        claim_next_local(&mut state, owner, lease_ms, now_ms, capabilities)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let capabilities = installed_worker_credential_capabilities(worker)?;
        let Some(run_id) = select_where(&state, now_ms, Some(worker), |row| {
            if row.cancellation_requested {
                return row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                    can_admit_claim_credentials(
                        &row.request,
                        &capabilities,
                        epoch,
                        now_ms,
                        ClaimCredentialOperation::Cancel,
                    )
                });
            }
            can_assign(
                worker,
                &row.request.placement,
                row.assignment.as_ref(),
                row.sandbox.is_some(),
                now_ms,
            )
            .is_ok()
                && row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                    can_admit_claim_credentials(
                        &row.request,
                        &capabilities,
                        epoch,
                        now_ms,
                        ClaimCredentialOperation::Execute,
                    )
                })
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

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let now_ms = crate::clock::normalize_millis(now_ms);
        let Some(run_id) = state.order.iter().find_map(|run_id| {
            state.rows.get(run_id).and_then(|row| {
                (retry_exhaustion_eligible(
                    row.state,
                    row.lease.as_ref().map(|lease| lease.expires_ms()),
                    row.attempt_count,
                    max_attempts,
                    now_ms,
                ) && row.active_attempt.is_none())
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
}

#[async_trait]
impl awaken_dispatch_contract::DispatchEnqueue for MemoryDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        enqueue_with_local(&mut state, request, options)
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaimOwnership for MemoryDispatchStore {
    /// The store's own clock decides liveness, exactly as it does for every
    /// other deadline this backend judges. This used to ride the trait default
    /// on a caller-supplied `now_ms`; the caller no longer has a say in when
    /// "now" is for rows this store owns.
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let now_ms = self.clock.now_ms();
        Ok(self
            .lock_commit_epoch(claim)
            .await?
            .is_some_and(|guard| guard.is_live_at(now_ms)))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchSettlement for MemoryDispatchStore {
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        self.settle_dispatch(run_id, epoch, outcome, consumed).await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchMaintenance for MemoryDispatchStore {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut operations = Vec::new();
        for row in state.rows.values_mut() {
            let expired = row.state == DispatchState::Leased
                && crate::clock::deadline_has_passed(
                    row.lease.as_ref().map(|l| l.expires_ms()),
                    expired_before_ms,
                );
            // An occupied physical slot no longer short-circuits the operator
            // route; the shared kernel below decides whether this quarantine
            // may take that occupied slot.
            if !expired || row.attempt_count < max_attempts {
                continue;
            }
            {
                let lease = row
                    .lease
                    .clone()
                    .expect("an expired running row carries its lease");
                let claim = RunClaim::from(&lease);
                // One shared kernel decides the slot disposition here too, so the
                // in-memory backend cannot drift from SQLite/Postgres.
                let slot = PhysicalAttemptSlot::from_facts(
                    row.active_attempt.is_some(),
                    row.active_attempt.is_some(),
                    row.active_attempt.as_ref() == Some(&claim),
                    row.active_attempt.as_ref() == Some(&claim),
                );
                if !slot.permits_settlement()
                    && !slot.quarantine_admits_occupation(row.cancellation_requested)
                {
                    continue;
                }
                let claim_epoch = claim.epoch;
                operations.push(DispatchOperation::LeaseLost {
                    claim: claim.clone(),
                    reason: LeaseLossReason::RetryExhausted,
                });
                operations.push(DispatchOperation::DeadLettered {
                    claim,
                    attempt_count: row.attempt_count,
                });
                let GuardedTransition::Applied(next) =
                    row.transition().exhaust_retries(claim_epoch, true)
                else {
                    continue;
                };
                row.apply_transition(next);
                row.lease = None;
                row.dead_lettered_at = Some(expired_before_ms);
            }
        }
        let quarantined = operations.len() / 2;
        for operation in operations {
            push_operation(&mut state, operation);
        }
        Ok(quarantined)
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.purge_dead(|_| true)
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.purge_dead(|row| row.dead_lettered_at.is_some_and(|at| at <= cutoff_ms))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchReads for MemoryDispatchStore {
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
        state
            .order
            .iter()
            .filter_map(|run| state.rows.get(run).map(|row| row.summary(run)))
            .collect()
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchAdministration for MemoryDispatchStore {
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        match state.rows.get_mut(run_id) {
            Some(row) if row.transition().requeue_dead_letter().is_some() => {
                let next = row
                    .transition()
                    .requeue_dead_letter()
                    .expect("the guarded transition remains stable");
                row.apply_transition(next);
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
        let transition = row.transition().cancel().map_err(crate::transition_error)?;
        let CancelTransition::Applied {
            state: next,
            revoked_lease,
            clear_retry_deadline,
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
            if clear_retry_deadline {
                row.retry_not_before_ms = None;
            }
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
}

#[async_trait]
impl awaken_dispatch_contract::DispatchAttemptControl for MemoryDispatchStore {
    async fn finish_attempt_closure(
        &self,
        record: &crate::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(
            &record.attempt().claim,
            crate::dispatch::AttemptFinish::Independent(record),
        )
        .await
    }

    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&AttemptExecutionBinding>,
        guard: awaken_worker_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<AttemptAdmission, DispatchError> {
        crate::application::admit_with_incarnation_guard(self, claim, binding, guard, handoff).await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlReads for MemoryDispatchStore {
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_agent_contract::credential::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let state = lock(&self.state)?;
        Ok(credential_binding::current(
            &state, claim, candidate, now_ms,
        ))
    }

    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let state = lock(&self.state)?;
        let mut depths = DispatchDepths {
            runnable: 0,
            leased: 0,
        };
        for (run_id, row) in &state.rows {
            if runnable(&state, run_id, now_ms).is_some() {
                depths.runnable += 1;
            } else if matches!(
                row.state,
                DispatchState::ReservationLeased | DispatchState::Leased
            ) && row
                .lease
                .as_ref()
                .is_some_and(|lease| lease.expires_ms() > now_ms)
            {
                depths.leased += 1;
            }
        }
        Ok(Some(depths))
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError> {
        lock(&self.state)?
            .rows
            .get(run_id)
            .map(Row::physical_attempt)
            .transpose()
            .map(Option::flatten)
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
            .filter(|completion| completion.receipt.sequence > after_sequence)
            .take(limit)
            .map(|completion| completion.receipt.clone())
            .collect())
    }

    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .rows
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .take(limit)
            .map(|(run_id, _)| run_id.clone())
            .collect())
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        lock(&self.state)?
            .rows
            .get(run_id)
            .map(|row| row.summary(run_id))
            .transpose()
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionRunAdmissionReads for MemoryDispatchStore {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        let state = lock(&self.state)?;
        let (live, completed) = stored_run_evidence(&state, run_id);
        if let Some(row) = live {
            crate::dispatch::validate_observed_dispatch_key(
                run_id,
                row.request.thread_id(),
                &row.request,
            )?;
            return Ok(SessionRunAdmissionObservation::Live(Box::new(
                row.request.clone(),
            )));
        }
        Ok(
            completed.map_or(SessionRunAdmissionObservation::Missing, |row| {
                SessionRunAdmissionObservation::Completed {
                    completion: row.receipt.clone(),
                    retained_request: row
                        .retained_request
                        .as_ref()
                        .map(|request| {
                            crate::dispatch::validate_retained_completion_request(
                                &row.receipt,
                                request,
                            )?;
                            Ok(request.clone())
                        })
                        .transpose(),
                }
            }),
        )
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionRunReservations for MemoryDispatchStore {
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let lease_ms = crate::dispatch::validate_pending_session_run_adoption(
            &expected_original,
            &command_fingerprint,
            owner,
            lease_ms,
        )?;
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get(expected_original.run_id()) else {
            return Ok(None);
        };
        let physical = row.physical_attempt()?;
        let adopted = crate::dispatch::pending_session_run_adoption(
            &row.request,
            &expected_original,
            row.transition(),
            row.lease.is_none()
                && row.assignment.is_none()
                && row.credential_bindings.is_empty()
                && row.credential_receipts.is_empty(),
            if physical.is_none() {
                PhysicalAttemptSlot::Empty
            } else {
                PhysicalAttemptSlot::OtherClaim
            },
            row.active_attempt_closing,
            command_fingerprint,
        )?;
        let Some((request, next)) = adopted else {
            return Ok(None);
        };
        let lease = DispatchLease::from_persisted(
            request.run_id().clone(),
            owner.to_string(),
            crate::clock::deadline_millis(self.clock.now_ms(), lease_ms),
            next.lease_epoch,
        )?;
        let claim = RunClaim::from(&lease);
        let row = state
            .rows
            .get_mut(request.run_id())
            .expect("the locked adoption row exists");
        row.request = request;
        row.apply_transition(next);
        row.lease = Some(lease);
        row.reservation_deadline_ms = None;
        Ok(Some(claim))
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let (request, reservation_ttl_ms) =
            validate_session_run_reservation_request(request, reservation_ttl_ms)?;
        let reservation_deadline_ms =
            crate::clock::deadline_millis(self.clock.now_ms(), reservation_ttl_ms);
        let run_id = request.run_id().clone();
        let (live, completed) = stored_run_evidence(&state, &run_id);
        if let Some(row) = live {
            return Ok(classify_live_session_run_reservation(
                &row.request,
                row.state,
                &request,
            ));
        }
        if let Some(completion) = completed {
            return Ok(classify_completed_session_run_reservation(
                completion.receipt.request_fingerprint.as_deref(),
                &request,
            ));
        }
        let options = SubmitOptions {
            supersede: request.session_run_replacement.supersedes_prior(),
            ..Default::default()
        };
        enqueue_new_local(&mut state, request, options)?;
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
            .any(|completion| &completion.receipt.run_id == run_id);
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
        let removable = state.rows.get(run_id).is_some_and(|row| {
            row.lease.is_none()
                && matches!(
                    row.transition().reject_reservation(),
                    GuardedTransition::Removed
                )
        });
        if !removable {
            return Ok(false);
        }
        state.rows.remove(run_id);
        state.order.retain(|candidate| candidate != run_id);
        state
            .pending
            .retain(|pending| &pending.input.run_id != run_id);
        Ok(true)
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
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::SessionReservationAuthority for MemoryDispatchStore {
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_state(claim, DispatchState::ReservationLeased)
            .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlClaims for MemoryDispatchStore {
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let mut policy_error = None;
        let capabilities = installed_worker_credential_capabilities(requester)?;
        let run_id = select_where(&state, now_ms, Some(requester), |row| {
            if row.cancellation_requested {
                return row.lease_epoch.checked_add(1).is_some_and(|epoch| {
                    can_admit_claim_credentials(
                        &row.request,
                        &capabilities,
                        epoch,
                        now_ms,
                        ClaimCredentialOperation::Cancel,
                    )
                });
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
                            can_admit_claim_credentials(
                                &row.request,
                                &capabilities,
                                epoch,
                                now_ms,
                                ClaimCredentialOperation::Execute,
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

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
}

#[async_trait]
impl DispatchOperationalFeed for MemoryDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        let events = lock(&self.state)?.operation_page(
            cursor,
            limit,
            run,
            crate::dispatch::OperationReadPurpose::Diagnostics,
        );
        Ok(crate::dispatch::diagnostic_page(cursor, events))
    }
}

use awaken_dispatch_contract::DispatchCommitAuthority as _;

#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchAttemptRecordReads for MemoryDispatchStore {
    async fn attempt_admissions_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        session_thread: &ThreadId,
        member_threads: &[ThreadId],
    ) -> Result<DispatchPage, DispatchError> {
        crate::dispatch::validate_admission_scope(session_thread, member_threads)?;
        let events = lock(&self.state)?.operation_page(
            cursor,
            limit,
            None,
            crate::dispatch::OperationReadPurpose::Admissions {
                session_thread,
                member_threads,
            },
        );
        Ok(crate::dispatch::diagnostic_page(cursor, events))
    }

    async fn attempt_closure_records_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<crate::AttemptClosurePage, DispatchError> {
        self.read_retained_attempt_closures(cursor, limit, run)
            .await
    }
}

#[cfg(test)]
mod admission_observation_tests {
    use super::MemoryDispatchStore;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_dispatch_contract::{DispatchError, SessionRunAdmissionReads};

    #[tokio::test]
    async fn admission_observation_propagates_poison_without_false_absence() {
        // O9: poisoned original State lock -> original Unavailable, no unwind
        // and no fabricated Missing. Integration callers cannot poison private
        // State, so this one owner-local negative guard complements the real
        // backend/conformance cases without adding a public injection port.
        let store = MemoryDispatchStore::new(awaken_time::system_clock());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store
                .state
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"));
            panic!("poison original dispatch State for O9");
        }));
        assert!(poisoned.is_err());
        assert!(
            matches!(store.observe_session_run_admission(&RunId("missing".into())).await,
            Err(DispatchError::Unavailable(message)) if message == "dispatch store poisoned")
        );
    }
}
