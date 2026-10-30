// @formal entrypoint ingress.pending_input_exactly_once
// @formal entrypoint ingress.settle_fence
// @formal entrypoint remote_worker.maintenance_scheduler_respects_adapter_ownership
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_precedes_activity
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
// @formal evidence ingress.epoch_reclaim
// @formal evidence ingress.single_owner
// @formal evidence run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
/// Remote claim-fenced checkpoint and committed-recovery operations. This is
/// not a second checkpoint store: the Coordinator endpoint applies the same
/// Dispatch claim guard to its canonical stores.
#[async_trait]
pub trait ClaimedCheckpointTransport: Send + Sync {
    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, DispatchError>;

    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError>;

    async fn delete_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError>;

    async fn load_recovery_snapshot(
        &self,
        claim: &RunClaim,
        session_work_authority: Option<
            &awaken_session_application_contract::SessionWorkAuthorityReceipt,
        >,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>;
}

#[async_trait]
pub trait ClaimedCredentialDeliveryTransport: Send + Sync {
    async fn refresh_credential_delivery(
        &self,
        claim: &RunClaim,
        request: awaken_credential_contract::AttemptCredentialDeliveryRequest,
    ) -> Result<Option<awaken_credential_contract::AttemptCredentialDelivery>, DispatchError>;
}

/// One operational observation from the canonical Dispatch store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchDepths {
    pub runnable: u64,
    pub leased: u64,
}

impl DispatchDepths {
    #[must_use]
    pub const fn worker_demand(self) -> u64 {
        self.runnable.saturating_add(self.leased)
    }
}

// Worker and Coordinator roles are independent required ports. Owned bundles
// live with their consumers; transports expose only the roles they implement.

/// Durable submission, with canonical replay identity and admission options.
#[async_trait]
pub trait DispatchEnqueue: Send + Sync {
    /// Idempotently record an accepted run at default options. Re-enqueueing the
    /// same run id and exact canonical dispatch is a no-op; reusing that id for a
    /// different dispatch is rejected, so an at-least-once submit has one exact
    /// effect per run.
    async fn enqueue(&self, request: RunDispatch) -> Result<(), DispatchError> {
        self.enqueue_with(request, SubmitOptions::default()).await
    }

    /// Record an accepted run with dispatch options (priority, dedupe key). A
    /// dedupe key already live makes this a no-op. On an exact canonical Run-id
    /// replay the first accepted options remain authoritative; retry options do
    /// not mutate or supersede the existing dispatch.
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError>;
}

/// Coordinator cancellation and operator requeue commands.
#[async_trait]
pub trait DispatchAdministration: Send + Sync {
    /// Durably request cancellation of a pending, awaiting, running, or
    /// dead-lettered dispatch.
    /// This operation records intent but never removes the row or pending input;
    /// for a running row it also advances the epoch and releases the old lease so
    /// that owner's later commit is fenced. The worker claims it and commits the
    /// Runtime-owned terminal boundary before settlement removes delivery state;
    /// requires-action Session children use their specialized interruption
    /// boundary rather than a generic `Cancelled`. Repeating it is idempotent.
    /// Returns `None` only for a terminal queue state or unknown run.
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError>;

    /// Return an unfenced dead-lettered run to the queue at a fresh budget.
    /// Returns `true` if a dead-lettered run with that id was requeued. A
    /// terminal Session fence reuses the durable cancellation bit to make the
    /// retained dead letter permanently non-runnable.
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError>;
}

/// Worker claim acquisition and exact reservation-repair resolution.
#[async_trait]
pub trait DispatchClaims: Send + Sync {
    /// Resolve one exact recovery claim before executor entry. Admitted binds
    /// the activity epoch and publishes Pending, Rejected removes the unstarted
    /// row, and Retry returns it to Reserved. Stale claims are fenced in all
    /// cases.
    async fn resolve_claimed_session_run_reservation(
        &self,
        _claim: &RunClaim,
        _resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Atomically record and claim one newly admitted Run.
    ///
    /// Parent-mediated child creation uses this command so the process-wide
    /// dispatcher cannot claim the new row between a separate enqueue and exact
    /// claim. Existing rows remain idempotent only when their canonical dispatch
    /// is exact; a different payload under the same Run id is rejected before
    /// placement eligibility is considered. An exact existing runnable row may
    /// be claimed under the ordinary exact-claim rules; an already leased or
    /// settled row returns `None`. The returned lease is otherwise identical to
    /// [`claim`](Self::claim), including its fencing epoch.
    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    async fn claim_new_run_compatible(
        &self,
        _request: RunDispatch,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Atomically append one idempotent input and claim its exact Run.
    ///
    /// This is the resume-side twin of [`claim_new_run`](Self::claim_new_run): a
    /// general pool cannot observe the newly runnable awaiting row before the
    /// parent-mediated caller receives its lease. A duplicate `message_id` is a
    /// no-op, and the ordinary correlation, due-time, thread writer, recovery,
    /// lease, and epoch rules still apply.
    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    async fn deliver_and_claim_compatible(
        &self,
        _input: PendingInput,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Claim one runnable dispatch for `owner`, single owner per run: a fresh
    /// `pending` run, an awaiting run with pending input (a wake), or a running
    /// dispatch whose lease expired (recovery). An Awaiting Run remains the one
    /// open writer for its Thread: fresh peers stay queued until that exact row is
    /// resumed or cancelled. The candidate itself is excluded from this peer
    /// check, so its matching reply/cancellation can wake it. Returns `None` when
    /// nothing is runnable, and the run's current pending input in [`Claimed`].
    /// Broad selection prefers eligible rows without an occupied physical slot.
    /// Recovery with an unconfirmed predecessor remains discoverable when no
    /// independent runnable work exists; it cannot starve other Threads. This
    /// order also applies to compatible/placed claims and never clears a slot.
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Atomically claim only work compatible with the registered worker snapshot.
    /// Implementations must evaluate the shared compatibility kernel before the
    /// lease transition and persist the resulting assignment with that transition.
    async fn claim_compatible(
        &self,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Claim one specific runnable Run without consuming unrelated queue work.
    ///
    /// Parent-mediated child Runs use this operation after durably scheduling a
    /// known child identity. It applies exactly the same pending/wake/recovery,
    /// single-writer-per-thread, lease, and epoch rules as [`claim`](Self::claim);
    /// the only difference is selection. `None` means that Run is not currently
    /// runnable or another owner/thread execution blocks it.
    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Claim one strictly expired `running` dispatch at/above `max_attempts`.
    /// This is delivery authority, not Run truth: route the ordinary [`Claimed`]
    /// through the canonical Worker `Ended(Indeterminate)` + fenced `Done` path.
    /// A failed commit stays eligible after expiry; `None` proves no eligible row remains at serialization.
    async fn claim_retry_exhausted(
        &self,
        _owner: &str,
        _lease_ms: u64,
        _max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    async fn claim_run_compatible(
        &self,
        _run_id: &RunId,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;
}

/// Mutations scoped to one already-acquired Run claim.
#[async_trait]
pub trait DispatchExecution: Send + Sync {
    /// Exact old attempt reports its closed local entry (not remote stop).
    /// Occupancy remains and ordinary ACK/handoff are fenced thereafter.
    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError>;

    /// Extend the lease on one exact fenced claim, so a long run is not
    /// reclaimed by another node's recovery while it is still making progress.
    /// The same atomically-read Dispatch row distinguishes a current claim, the
    /// exact physical attempt whose durable cancellation was requested, and an
    /// unrelated/lost claim. Both non-current outcomes stop the holder; the typed
    /// cancellation arm preserves the reason without adding another signal
    /// authority. This is the multi-node liveness knob (ADR-0019/0024).
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError>;

    /// Acquire the physical executor slot for one exact, live claim.
    ///
    /// The slot is part of the existing Dispatch aggregate, not a second lease.
    /// A successfully acknowledged epoch cannot be admitted again. Consecutive
    /// in-process ScheduledActions belong inside the original physical attempt;
    /// another execution after ACK requires a newly claimed epoch.
    /// Reclaim may advance the durable claim epoch while the predecessor is
    /// still unwinding, but a successor receives [`AttemptAdmission::Blocked`]
    /// until [`finish_attempt`](Self::finish_attempt) records that predecessor's
    /// model/tool/Sandbox future has actually returned. Implementations must make
    /// an exact retry idempotent and return `Fenced` for a non-current claim.
    /// A supplied binding is frozen with the slot in this same transaction and
    /// must name the exact registered assignment. Same-claim retries preserve
    /// it, including None; coordinates cannot be backfilled after execution.
    async fn begin_attempt(
        &self,
        _claim: &RunClaim,
        _binding: Option<&crate::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError>;

    /// Atomically hand a predecessor's physical slot to this exact current
    /// claim after the selected executor proved it can continue the already
    /// committed remote operation without submitting its initiating effect.
    ///
    /// This is not crash/lease expiry. Callers without such durable executor
    /// evidence must use [`begin_attempt`](Self::begin_attempt) and remain
    /// blocked until predecessor quiescence. Empty and exact slots are handled
    /// idempotently; a non-current claim is fenced.
    async fn handoff_attempt(
        &self,
        _claim: &RunClaim,
        _binding: Option<&crate::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError>;

    /// Acknowledge complete physical quiescence owned by `claim`: either actual
    /// executor return without nested uncertainty, or the Coordinator's trusted
    /// independent complete-termination assertion. A local process exit alone
    /// is insufficient. This intentionally accepts the exact predecessor epoch
    /// after a durable reclaim: it can only clear its own execution slot and grants no
    /// commit, checkpoint, or settlement authority. A mismatched claim is fenced.
    /// A supplied cause retains only the original local observation, never
    /// Provider/Session closure; missing legacy evidence is not backfilled.
    async fn finish_attempt(
        &self,
        _claim: &RunClaim,
        cause: Option<super::AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Return an exact, still-current claim to `pending` without consuming its
    /// crash retry budget.
    ///
    /// This is the pre-execution admission rollback: a Worker may win the Run
    /// queue and then discover that a subordinate authority (for example an
    /// Environment's single active Session Work lease) is temporarily busy.
    /// The complete claim fences the rollback, so a stale Worker cannot release
    /// a replacement owner's Run. An exact physical attempt owned by this claim
    /// still fences rollback, but a predecessor's independently persisted slot
    /// does not: the row remains leased with an expired deadline so the next
    /// claimant recovers it rather than treating it as fresh Pending work. The
    /// stale slot, pending input and attempt count are preserved; same-Thread
    /// successors remain excluded until the old execution responsibility ends.
    async fn relinquish_claim(&self, _claim: &RunClaim) -> Result<SettleOutcome, DispatchError>;

    /// Return one exact current claim to `pending` and make it ineligible for
    /// ordinary claim until the store's clock reaches `retry_after_ms` from now.
    ///
    /// This is a request-local resolution retry, not a Worker-wide failure gate,
    /// and not a failed attempt: the claim waited on capacity or on a busy
    /// subordinate authority. A failed attempt uses
    /// [`retry_claim`](Self::retry_claim) instead.
    /// Implementations preserve request bytes, priority, retry budget, pending
    /// input and any independent physical-attempt slot. Cancellation and terminal
    /// recovery must bypass the delay. The store owns both the current time and
    /// the atomic claim/deadline transition.
    async fn defer_claim(
        &self,
        _claim: &RunClaim,
        _retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Return one exact current claim after a classified failed attempt,
    /// spending one attempt of the dispatch retry budget (ADR-0015 D1).
    ///
    /// The fence, physical-slot rule, retry deadline and preserved request,
    /// priority and pending input are those of
    /// [`defer_claim`](Self::defer_claim). Below `max_attempts` the row
    /// returns to `pending` with one more attempt. The attempt that reaches
    /// `max_attempts` leaves the row `running` under an already expired lease,
    /// so [`DispatchClaims::claim_retry_exhausted`] terminalizes it exactly as
    /// it does a crashed attempt; the retry deadline still holds back ordinary
    /// claims. Behind a predecessor's occupied slot the row stays in recovery,
    /// and that recovery re-claim spends the attempt. `max_attempts` is the
    /// caller's retry policy, as for `claim_retry_exhausted`; a remote
    /// transport applies its Coordinator's own policy instead.
    async fn retry_claim(
        &self,
        _claim: &RunClaim,
        _retry_after_ms: u64,
        _max_attempts: u64,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Persist secret-free proof that the exact claim-epoch credential binding
    /// was realized. Implementations fence on the complete claim, verify through
    /// [`verify_credential_realization_receipt`], and make exact retries
    /// idempotent. A stale claim returns [`SettleOutcome::Fenced`].
    async fn record_credential_realization(
        &self,
        _claim: &RunClaim,
        _receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Bind a currently claimed run to the sandbox it was placed on (B-P3,
    /// ADR-0021 §6). The complete claim is required so a stale incarnation cannot
    /// overwrite the replacement's environment after its lease expires. The
    /// reference is opaque to the dispatch aggregate (the fleet serializes a
    /// `SandboxHandle` into it). Stored durably so `claim` returns it on recovery
    /// and `reconcile_adoption` can re-adopt the same sandbox. Default is a no-op
    /// for backends that do not persist the binding (the neutral seam).
    async fn bind_sandbox(
        &self,
        _claim: &RunClaim,
        _sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        Ok(SettleOutcome::Fenced)
    }
}

/// Read authority for the current Run claim.
#[async_trait]
pub trait DispatchClaimOwnership: Send + Sync {
    /// Whether one exact fenced claim still owns a live dispatch lease.
    ///
    /// The default reuses the backend's authoritative epoch guard instead of
    /// introducing another claim source or duplicating owner/epoch queries.
    /// Required, not defaulted: the answer is "is this claim live *now*", and
    /// only the implementation knows which clock its rows are judged against.
    /// A default would have to invent one or take it from the caller, and a
    /// caller's clock cannot decide liveness for rows other nodes also claim.
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError>;
}

/// Local durable claim guards for cross-aggregate commits and resource effects.
#[async_trait]
pub trait DispatchCommitAuthority: Send + Sync {
    /// Return the current live claim only when this exact registered Worker
    /// incarnation owns `run_id` and no cancellation has been requested.
    ///
    /// Server-side application capability issuers use this shape because they
    /// authenticate a Worker identity and Run id, but must not trust a
    /// caller-supplied claim epoch.
    async fn worker_owns_run(
        &self,
        _identity: &WorkerIdentity,
        _run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError>;

    /// Hold a claim's exact epoch stable across a `ThreadCommit`.
    ///
    /// `Some` means the claim remains authoritative while the guard lives;
    /// `None` means the row is gone or its epoch/owner no longer matches. This is
    /// required: a durable adapter may not degrade to a check-then-commit sequence.
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError>;
}

/// Exact-epoch settlement after committed Run truth is observed.
///
/// Use the Dispatch grant's generation:
/// ```
/// use awaken_dispatch_contract::{DispatchLease, DispatchOutcome, DispatchSettlement};
/// fn settle_with_dispatch_epoch(dispatch: &dyn DispatchSettlement, lease: &DispatchLease) {
///     let pending = dispatch.settle(lease.run_id(), lease.epoch(), DispatchOutcome::Done, &[]);
///     drop(pending);
/// }
/// ```
/// An Environment Work grant cannot accidentally settle that Dispatch:
/// ```compile_fail,E0308
/// use awaken_agent_contract::RunId;
/// use awaken_dispatch_contract::{DispatchOutcome, DispatchSettlement};
/// use awaken_session_application_contract::SessionWorkGrant;
/// fn work_epoch_cannot_settle_dispatch(dispatch: &dyn DispatchSettlement, run: &RunId, work: &SessionWorkGrant) {
///     let pending = dispatch.settle(run, work.epoch(), DispatchOutcome::Done, &[]);
///     drop(pending);
/// }
/// ```
#[async_trait]
pub trait DispatchSettlement: Send + Sync {
    /// Settle a claimed dispatch, fenced by the lease `epoch` the caller holds (from
    /// [`Claimed`]`.lease.epoch`). The settle applies only when `epoch` is still the
    /// row's current epoch; if the run was re-claimed under a higher epoch (a
    /// reclaimer took the lapsed lease), the settle is rejected as
    /// [`SettleOutcome::Fenced`] and NOTHING is changed — a stale owner can never
    /// clobber the current owner's in-flight dispatch (reset its lease, re-await it,
    /// or delete it out from under an active drive).
    ///
    /// When applied: `Done` removes the dispatch and all its pending input; `Awaiting`
    /// returns it to the awaiting state and drops only the `consumed` pending (by
    /// `message_id`), leaving input that arrived mid-attempt for the next wake.
    /// `Awaiting` also resets the crash-retry budget — a run that reaches a checkpoint
    /// refreshes its attempts.
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError>;
}

/// Coordinator operational queries over persisted dispatch rows.
#[async_trait]
pub trait DispatchReads: Send + Sync {
    /// The run ids currently dead-lettered, for operations.
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError>;

    /// The run ids superseded by a newer submission on their thread (ADR-0022),
    /// for operations — the mirror of [`dead_letters`](Self::dead_letters).
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Every dispatch row's operational summary, in enqueue order — the query
    /// surface for monitoring and maintenance (ADR-0025).
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError>;
}

/// Coordinator quarantine and garbage-collection commands.
#[async_trait]
pub trait DispatchMaintenance: Send + Sync {
    /// Manually quarantine expired crashed dispatches at/above `max_attempts`.
    /// Moves rows to `DeadLetter`; automatic services use
    /// [`claim_retry_exhausted`](DispatchClaims::claim_retry_exhausted), so the Run first
    /// receives committed terminal truth (ADR-0015).
    ///
    /// `expired_before_ms` is the one time on this port a caller still supplies,
    /// and it is not a clock: this route is operator-driven, and the operator
    /// chooses the cutoff ("quarantine what was already expired at T"). Naming it
    /// `now_ms` is what let an implementation quietly answer with its own "now"
    /// instead of the cutoff it was given, which is a different question.
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError>;

    /// Remove dead-lettered dispatches whose physical slot is actually empty,
    /// together with their pending input. Occupied, unbound, closing or corrupt
    /// physical facts retain the original row; GC never certifies execution exit.
    /// Returns how many were purged.
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError>;

    /// Apply the same empty-slot requirement to dead-lettered dispatches whose
    /// dead-letter time is at or before `cutoff_ms`. This is the time-windowed
    /// GC the daemon runs on a cadence (ADR-0023). Returns how many were purged.
    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError>;
}

/// Read the original Run request or completion from its Coordinator authority.
/// This role grants neither reservation mutation nor recovery admission.
///
/// ```compile_fail,E0599
/// use awaken_dispatch_contract::{RunDispatch, SessionRunAdmissionReads};
/// async fn mutate(reader: &dyn SessionRunAdmissionReads, request: RunDispatch) {
///     reader.reserve_session_run(request, 1).await;
/// }
/// ```
#[async_trait]
pub trait SessionRunAdmissionReads: Send + Sync {
    /// Observe this exact Run's original request or retained completion in one
    /// store snapshot. This never claims, writes, consumes a cursor or authorizes
    /// replay. Missing is not historical nonexecution; legacy absent fields stay
    /// absent. Invalid data/storage failures return their original error rather
    /// than Missing. Observation errors are uncertainty, not definitive admission
    /// rejection. A caller must bind the data to its original command and still
    /// use the ordinary exact admission transaction to resolve subsequent races.
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<SessionRunAdmissionObservation, DispatchError>;
}

/// Coordinator reservation and Session-child admission commands.
#[async_trait]
pub trait SessionRunReservations: SessionReservationAuthority + SessionRunAdmissionReads {
    /// Persist one complete self-affine Session Run intent without making it
    /// claimable. The store applies this TTL using its own clock, granting an
    /// exclusive window to commit the Session activity and activate this row.
    /// Exact Run-id replay is idempotent; another payload is rejected.
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError>;

    /// Exclusively adopt one exact legacy ordinary Pending request for Session
    /// activity admission. This grants only a `ReservationLeased` epoch: the
    /// caller must guard the authoritative no-Run read and Session receipt CAS,
    /// release that guard, then use the existing reservation resolution. No
    /// Worker assignment, credential/physical effect, or Run completion is made.
    /// Another payload, state, existing authority, or missing row yields no
    /// tenure. The original frozen execution inputs are never reconstructed.
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError>;

    /// Publish one still-unleased reservation after the Session root committed
    /// its exact nonzero activity epoch. The request and state update are one
    /// dispatch transaction; exact replay after activation is a no-op.
    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError>;

    /// Remove one unleased reservation after deterministic Session rejection.
    /// A row that already activated or acquired a recovery claim is fenced.
    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError>;

    /// Admit one ordinary child Run while atomically enforcing a parent-Session
    /// bound over distinct, unarchived child Threads.
    ///
    /// The dispatch's own Thread remains its lifecycle identity; the required
    /// `session_thread_id` is only its parent affinity. Exact canonical Run-id
    /// replays are no-ops and consume no new slot. Both live rows and completion
    /// tombstones count; only the trusted committed archive evidence releases a
    /// Thread slot. Implementations serialize the capacity decision with
    /// insertion so concurrent first admissions cannot exceed the policy bound.
    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError>;
}
/// Exact reservation-epoch authority for activity admission and repair.
#[async_trait]
pub trait SessionReservationAuthority: Send + Sync {
    /// Acquire the narrow authority for one live `ReservationLeased` repair.
    /// This guard may authorize only Session activity admission; commit, stream,
    /// sandbox, credential, and settle operations continue to require an
    /// ordinary `Leased` claim through [`DispatchCommitAuthority::lock_commit_epoch`].
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError>;
}

/// Trusted Coordinator attempt admission and independent closure commands.
#[async_trait]
pub trait DispatchAttemptControl: Send + Sync {
    /// Trusted Coordinator-only retirement after complete independent closure
    /// evidence. Worker transports expose only ordinary return and entry close.
    async fn finish_attempt_closure(
        &self,
        record: &AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Use the original Registry exclusion through physical admission. SQL
    /// adapters sharing one pool transfer its transaction instead of requiring
    /// a second connection; other adapters retain the guard until completion.
    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&crate::dispatch::AttemptExecutionBinding>,
        guard: awaken_worker_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<AttemptAdmission, DispatchError>;
}

/// Coordinator placement and terminal-recovery claim acquisition.
#[async_trait]
pub trait DispatchControlClaims: Send + Sync {
    /// Claim according to a replaceable preference policy while preserving the
    /// same atomic eligibility, recovery and assignment transition. Preference
    /// may use a liveness snapshot, while the backend's final claim transition
    /// rechecks immutable eligibility and fencing; a stale preference can delay
    /// work but cannot widen execution authority or create two owners.
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Claim one quiescent unleased `pending`/`awaiting` row or expired `running`
    /// lease after the committed Run authority has already proved that the same
    /// `run_id` is terminal. A `pending` row is the canonical retry shape after
    /// a post-commit settlement failure released its mutation claim. If that
    /// release set a durable retry deadline, terminal recovery observes the
    /// same deadline as an ordinary claim.
    ///
    /// This is the repair-side entry into the ordinary fenced settlement path:
    /// it never reads or accepts Run outcome truth, never executes the Run, and
    /// skips execution placement/credential admission because the caller will
    /// only redeliver terminal observers and call [`settle`](DispatchSettlement::settle) with
    /// `Done`. Implementations must claim only an unleased `pending`/`awaiting`
    /// row whose Thread has no running dispatch, or a `running` row whose lease
    /// is strictly expired. Every state requires an empty physical slot: this
    /// maintenance entry cannot execute or observe an occupied predecessor.
    /// A live lease, occupied, not-yet-due, missing, superseded, or dead-lettered
    /// row returns `None`.
    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;
}

/// Original physical-attempt records; a read role, never closure authority.
#[async_trait]
pub trait DispatchAttemptRecordReads: Send + Sync {
    /// Complete original admission inventory selected by a frozen Session root
    /// or member Thread, before the logical page limit. Parent and member matches
    /// are a union: the consumer must still reject inconsistent affinity. The
    /// view retains original global cursors and the store's committed-prefix
    /// barrier; it grants neither closure nor execution authority.
    /// Storage failures remain unknown, never a fabricated empty inventory.
    async fn attempt_admissions_after(
        &self,
        cursor: crate::DispatchCursor,
        limit: usize,
        session_thread: &ThreadId,
        member_threads: &[ThreadId],
    ) -> Result<crate::DispatchPage, DispatchError>;

    /// Read retained ordinary acknowledgements and independent closure facts from the one original operation
    /// storage. Missing records remain unknown; these are not an inventory of
    /// all old execution or a cleanup/placement grant. Filtering precedes the
    /// logical page limit and the original global cursor is retained.
    async fn attempt_closure_records_after(
        &self,
        cursor: crate::DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<AttemptClosurePage, DispatchError>;
}

/// Coordinator bounded recovery, attempt, completion and demand reads.
#[async_trait]
pub trait DispatchControlReads: DispatchAttemptRecordReads {
    /// Read one exact persisted binding only while `claim` remains the current
    /// owner. This query grants no material and changes no delivery state.
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_agent_contract::credential::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError>;

    /// Read the existing occupied physical identity, independently of the newer
    /// mutation claim/Worker assignment. No transport authority or stop evidence
    /// is minted by this read. Unsupported adapters must error, never return a
    /// misleading empty slot.
    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError>;

    /// Return durable applied-`Done` facts after `after_sequence`, in ascending
    /// sequence order, capped at `limit`.
    ///
    /// Native durable stores retain these rows as permanent run-id tombstones;
    /// completed ids therefore cannot be re-enqueued after their live dispatch
    /// row is removed (ADR-0060). A transport that does not expose this
    /// server-local projection query fails explicitly.
    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError>;

    /// One store-time observation of claimable and currently leased Dispatch
    /// rows. The two classes are disjoint: an expired lease is runnable, while
    /// a live reservation/execution lease still needs its Worker. Native stores
    /// return exact counts; composed/remote backends may return `None` until
    /// they expose an efficient server-side read. This is operational evidence,
    /// never claim or settlement authority.
    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError>;

    /// Preserve the exact backlog observation for existing queue consumers.
    /// The same store query owns both numbers, avoiding a second classification.
    async fn runnable_depth(&self) -> Result<Option<u64>, DispatchError> {
        Ok(self.dispatch_depths().await?.map(|depths| depths.runnable))
    }

    /// Bounded keyset read of current Dispatch identities, in ascending Run-id
    /// order strictly after `after`. A zero limit reads nothing. No lifecycle
    /// filtering or request decoding occurs here: each returned identity spends
    /// one caller inspection budget, including malformed or ineligible rows.
    /// The cursor is only a scheduling hint, not a snapshot or mutation right;
    /// callers wrap after an empty page and revalidate each exact row. Rows
    /// inserted behind the cursor are discovered on the next traversal.
    /// Backends must not implement this by materializing `list_dispatches`.
    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError>;

    /// Read one current operational projection without scanning its peers.
    /// Missing means this row is absent, not that its external effects stopped.
    /// Invalid durable data returns an error rather than an absent summary.
    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError>;
}
