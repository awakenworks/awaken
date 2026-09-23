/// Durable run-dispatch queue: activation opportunity, claim, lease, recovery.
#[async_trait]
pub trait DispatchQueue: Send + Sync {
    /// Whether this adapter owns process-local queue maintenance (outbox relay,
    /// quarantine GC and global terminal scans). Database-less transports do
    /// not: the upstream Coordinator owns those operations. This is immutable
    /// adapter topology, not a grant to execute or mutate a claimed Run.
    fn owns_local_maintenance(&self) -> bool {
        true
    }

    /// Resolve one exact recovery claim before executor entry. Admitted binds
    /// the activity epoch and publishes Pending, Rejected removes the unstarted
    /// row, and Retry returns it to Reserved. Stale claims are fenced in all
    /// cases.
    async fn resolve_claimed_session_run_reservation(
        &self,
        _claim: &RunClaim,
        _resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError>;

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
    async fn finish_attempt(&self, _claim: &RunClaim) -> Result<SettleOutcome, DispatchError>;

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
    /// This is a request-local resolution retry, not a Worker-wide failure gate.
    /// Implementations preserve request bytes, priority, retry budget, pending
    /// input and any independent physical-attempt slot. Cancellation and terminal
    /// recovery must bypass the delay. The store owns both the current time and
    /// the atomic claim/deadline transition.
    async fn defer_claim(
        &self,
        _claim: &RunClaim,
        _retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError>;

    /// Whether one exact fenced claim still owns a live dispatch lease.
    ///
    /// The default reuses the backend's authoritative epoch guard instead of
    /// introducing another claim source or duplicating owner/epoch queries.
    /// Required, not defaulted: the answer is "is this claim live *now*", and
    /// only the implementation knows which clock its rows are judged against.
    /// A default would have to invent one or take it from the caller, and a
    /// caller's clock cannot decide liveness for rows other nodes also claim.
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError>;

    /// Persist secret-free proof that the exact claim-epoch credential binding
    /// was realized. Implementations fence on the complete claim, verify through
    /// [`verify_credential_realization_receipt`], and make exact retries
    /// idempotent. A stale claim returns [`SettleOutcome::Fenced`].
    async fn record_credential_realization(
        &self,
        _claim: &RunClaim,
        _receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError>;

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

    /// Hold a claim's exact epoch stable across a `ThreadCommit`.
    ///
    /// `Some` means the claim remains authoritative while the guard lives;
    /// `None` means the row is gone or its epoch/owner no longer matches. This is
    /// required: a durable adapter may not degrade to a check-then-commit sequence.
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError>;

    /// Manually quarantine expired crashed dispatches at/above `max_attempts`.
    /// Moves rows to `DeadLetter`; automatic services use
    /// [`claim_retry_exhausted`](Self::claim_retry_exhausted), so the Run first
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

    /// The run ids currently dead-lettered, for operations.
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Return an unfenced dead-lettered run to the queue at a fresh budget.
    /// Returns `true` if a dead-lettered run with that id was requeued. A
    /// terminal Session fence reuses the durable cancellation bit to make the
    /// retained dead letter permanently non-runnable.
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError>;

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

    /// Remove every dead-lettered dispatch (and its pending input) — operator GC.
    /// Returns how many were purged.
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError>;

    /// Remove dead-lettered dispatches whose dead-letter time is at or before
    /// `cutoff_ms` (and their pending input) — time-windowed GC the daemon runs on
    /// a cadence (ADR-0023). Returns how many were purged.
    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError>;

    /// The run ids superseded by a newer submission on their thread (ADR-0022),
    /// for operations — the mirror of [`dead_letters`](Self::dead_letters).
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Every dispatch row's operational summary, in enqueue order — the query
    /// surface for monitoring and maintenance (ADR-0025).
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError>;

    /// Claim-fenced checkpoint/recovery transport owned by a database-less
    /// Worker projection. Local durable stores return None and use their
    /// colocated checkpoint/recovery ports; callers select the path before I/O.
    fn claimed_checkpoint_transport(&self) -> Option<&dyn ClaimedCheckpointTransport> {
        None
    }

    fn claimed_credential_delivery_transport(
        &self,
    ) -> Option<&dyn ClaimedCredentialDeliveryTransport> {
        None
    }

    /// The Control-side surface of this queue: Session Run reservations,
    /// child admission, placement claims, terminal recovery, physical attempt
    /// readback, the completion feed, and operational scans. `None` means this
    /// handle is a Worker-side projection that must never perform them
    /// (ADR-0081 capability ladder); a composition root selects a
    /// [`ControlDispatchQueue`] instead of discovering the gap at the effect.
    fn control(&self) -> Option<&dyn ControlDispatchQueue> {
        None
    }
}

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
        session_work_authority: Option<&awaken_session_work_contract::SessionWorkAuthorityReceipt>,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>;
}

#[async_trait]
pub trait ClaimedCredentialDeliveryTransport: Send + Sync {
    async fn refresh_credential_delivery(
        &self,
        claim: &RunClaim,
        request: awaken_runtime_contract::AttemptCredentialDeliveryRequest,
    ) -> Result<Option<awaken_runtime_contract::AttemptCredentialDelivery>, DispatchError>;
}

/// Control-side dispatch operations that only the Coordinator's own store
/// performs: Session Run reservations and their epoch lock, Session child
/// admission, placement-aware and terminal-recovery claims, physical attempt
/// readback, the completion feed, and operational scans. Every durable store
/// implements this; a Worker-side transport projection does not, and says so
/// through [`DispatchQueue::control`] rather than a failing default.
#[async_trait]
pub trait ControlDispatchQueue: DispatchQueue {
    /// Read one exact persisted binding only while `claim` remains the current
    /// owner. This query grants no material and changes no delivery state.
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_runtime_contract::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError>;

    /// Persist one complete self-affine Session Run intent without making it
    /// claimable. The store applies this TTL using its own clock, granting an
    /// exclusive window to commit the Session activity and activate this row.
    /// Exact Run-id replay is idempotent; another payload is rejected.
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError>;

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
    /// `run_id` is terminal. A `pending` row is the canonical immediate-retry
    /// shape after a post-commit settlement failure released its mutation claim.
    ///
    /// This is the repair-side entry into the ordinary fenced settlement path:
    /// it never reads or accepts Run outcome truth, never executes the Run, and
    /// skips execution placement/credential admission because the caller will
    /// only redeliver terminal observers and call [`settle`](Self::settle) with
    /// `Done`. Implementations must claim only an unleased `pending`/`awaiting`
    /// row whose Thread has no running dispatch, or a `running` row whose lease
    /// is strictly expired. Every state requires an empty physical slot: this
    /// maintenance entry cannot execute or observe an occupied predecessor.
    /// A live lease, occupied, missing, superseded, or dead-lettered row returns `None`.
    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Read the existing occupied physical identity, independently of the newer
    /// mutation claim/Worker assignment. No transport authority or stop evidence
    /// is minted by this read. Unsupported adapters must error, never return a
    /// misleading empty slot.
    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError>;

    /// Acquire the narrow authority for one live `ReservationLeased` repair.
    /// This guard may authorize only Session activity admission; commit, stream,
    /// sandbox, credential, and settle operations continue to require an
    /// ordinary `Leased` claim through [`Self::lock_commit_epoch`].
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError>;

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

    /// Current number of dispatches that are claimable at `now_ms`. Native stores
    /// return an exact value; composed/remote backends may return `None` until they
    /// expose an efficient server-side count. This is an operations query only and
    /// never participates in scheduling correctness.
    async fn runnable_depth(&self) -> Result<Option<u64>, DispatchError>;

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
