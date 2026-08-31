/// Durable run-dispatch queue: activation opportunity, claim, lease, recovery.
#[async_trait]
pub trait DispatchQueue: Send + Sync {
    /// Persist one complete self-affine Session Run intent without making it
    /// claimable. The store applies this TTL using its own clock, granting an
    /// exclusive window to commit the Session activity and activate this row.
    /// Exact Run-id replay is idempotent; another payload is rejected.
    async fn reserve_session_run(
        &self,
        _request: RunDispatch,
        _reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support Session Run reservations".to_string(),
        ))
    }

    /// Publish one still-unleased reservation after the Session root committed
    /// its exact nonzero activity epoch. The request and state update are one
    /// dispatch transaction; exact replay after activation is a no-op.
    async fn activate_session_run_reservation(
        &self,
        _run_id: &RunId,
        _session_thread_id: &ThreadId,
        _session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support Session Run reservation activation".to_string(),
        ))
    }

    /// Remove one unleased reservation after deterministic Session rejection.
    /// A row that already activated or acquired a recovery claim is fenced.
    async fn reject_session_run_reservation(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support Session Run reservation rejection".to_string(),
        ))
    }

    /// Resolve one exact recovery claim before executor entry. Admitted binds
    /// the activity epoch and publishes Pending, Rejected removes the unstarted
    /// row, and Retry returns it to Reserved. Stale claims are fenced in all
    /// cases.
    async fn resolve_claimed_session_run_reservation(
        &self,
        _claim: &RunClaim,
        _resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support claimed Session Run reservation resolution".to_string(),
        ))
    }

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
        _request: RunDispatch,
        _admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support bounded Session-child admission".to_string(),
        ))
    }

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
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    async fn claim_new_run_compatible(
        &self,
        _request: RunDispatch,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support registered-worker exact claims".to_string(),
        ))
    }

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
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    async fn deliver_and_claim_compatible(
        &self,
        _input: PendingInput,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support registered-worker delivery claims".to_string(),
        ))
    }

    /// Claim one runnable dispatch for `owner`, single owner per run: a fresh
    /// `pending` run, an awaiting run with pending input (a wake), or a running
    /// dispatch whose lease expired (recovery). An Awaiting Run remains the one
    /// open writer for its Thread: fresh peers stay queued until that exact row is
    /// resumed or cancelled. The candidate itself is excluded from this peer
    /// check, so its matching reply/cancellation can wake it. Returns `None` when
    /// nothing is runnable, and the run's current pending input in [`Claimed`].
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Atomically claim only work compatible with the registered worker snapshot.
    /// Implementations must evaluate the shared compatibility kernel before the
    /// lease transition and persist the resulting assignment with that transition.
    async fn claim_compatible(
        &self,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support registered-worker claims".to_string(),
        ))
    }

    /// Claim according to a replaceable preference policy while preserving the
    /// same atomic eligibility, recovery and assignment transition. Preference
    /// may use a liveness snapshot, while the backend's final claim transition
    /// rechecks immutable eligibility and fencing; a stale preference can delay
    /// work but cannot widen execution authority or create two owners.
    async fn claim_placed(
        &self,
        _requester: &WorkerSnapshot,
        _workers: Vec<WorkerSnapshot>,
        _policy: std::sync::Arc<dyn PlacementPolicy>,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support policy-based registered-worker claims".to_string(),
        ))
    }

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
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Claim one quiescent `awaiting` row or expired `running` lease after the
    /// committed Run authority has already proved that the same `run_id` is
    /// terminal.
    ///
    /// This is the repair-side entry into the ordinary fenced settlement path:
    /// it never reads or accepts Run outcome truth, never executes the Run, and
    /// skips execution placement/credential admission because the caller will
    /// only redeliver terminal observers and call [`settle`](Self::settle) with
    /// `Done`. Implementations must claim only an unleased `awaiting` row whose
    /// Thread has no running dispatch, or a `running` row whose lease is strictly
    /// expired. A live lease, missing, pending, superseded, or dead-lettered row
    /// returns `None`.
    async fn claim_for_terminal_recovery(
        &self,
        _run_id: &RunId,
        _owner: &str,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support committed-terminal dispatch recovery".to_string(),
        ))
    }

    /// Claim one strictly expired `running` dispatch at/above `max_attempts`.
    /// This is delivery authority, not Run truth: route the ordinary [`Claimed`]
    /// through the canonical Worker `Ended(Indeterminate)` + fenced `Done` path.
    /// A failed commit stays eligible after expiry; `None` proves no eligible row remains at serialization.
    async fn claim_retry_exhausted(
        &self,
        _owner: &str,
        _lease_ms: u64,
        _now_ms: u64,
        _max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support retry-exhaustion terminal claims".to_string(),
        ))
    }

    async fn claim_run_compatible(
        &self,
        _run_id: &RunId,
        _worker: &WorkerSnapshot,
        _lease_ms: u64,
        _now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support registered-worker run claims".to_string(),
        ))
    }

    /// Extend the lease on one exact fenced claim, so a long run is not
    /// reclaimed by another node's recovery while it is still making progress.
    /// Returns `true` if the lease was renewed (the run is still owned by
    /// that owner *and epoch*); `false` if it was lost (stolen, settled, or
    /// unknown) — the holder should then stop. This is the multi-node liveness
    /// knob (ADR-0019/0024).
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError>;

    /// Acquire the physical executor slot for one exact, live claim.
    ///
    /// The slot is part of the existing Dispatch aggregate, not a second lease.
    /// Reclaim may advance the durable claim epoch while the predecessor is
    /// still unwinding, but a successor receives [`AttemptAdmission::Blocked`]
    /// until [`finish_attempt`](Self::finish_attempt) records that predecessor's
    /// model/tool/Sandbox future has actually returned. Implementations must make
    /// an exact retry idempotent and return `Fenced` for a non-current claim.
    async fn begin_attempt(
        &self,
        _claim: &RunClaim,
        _now_ms: u64,
    ) -> Result<AttemptAdmission, DispatchError> {
        Err(DispatchError::Rejected(
            "dispatch backend does not support physical attempt admission".to_string(),
        ))
    }

    /// Acknowledge that the physical executor future owned by `claim` has
    /// returned. This intentionally accepts the exact predecessor epoch after a
    /// durable reclaim: it can only clear its own execution slot and grants no
    /// commit, checkpoint, or settlement authority. A mismatched claim is fenced.
    async fn finish_attempt(&self, _claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "dispatch backend does not support physical attempt quiescence".to_string(),
        ))
    }

    /// Return an exact, still-current claim to `pending` without consuming its
    /// crash retry budget.
    ///
    /// This is the pre-execution admission rollback: a Worker may win the Run
    /// queue and then discover that a subordinate authority (for example an
    /// Environment's single active Session Work lease) is temporarily busy.
    /// The complete claim fences the rollback, so a stale Worker cannot release
    /// a replacement owner's Run. Pending input and attempt count are preserved.
    async fn relinquish_claim(&self, _claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "dispatch backend does not support claim relinquish".to_string(),
        ))
    }

    /// Whether one exact fenced claim still owns a live dispatch lease.
    ///
    /// The default reuses the backend's authoritative epoch guard instead of
    /// introducing another claim source or duplicating owner/epoch queries.
    async fn claim_is_current(&self, claim: &RunClaim, now_ms: u64) -> Result<bool, DispatchError> {
        Ok(self
            .lock_commit_epoch(claim)
            .await?
            .is_some_and(|guard| guard.is_live_at(now_ms)))
    }

    /// Acquire the narrow authority for one live `ReservationLeased` repair.
    /// This guard may authorize only Session activity admission; commit, stream,
    /// sandbox, credential, and settle operations continue to require an
    /// ordinary `Leased` claim through [`Self::lock_commit_epoch`].
    async fn lock_session_run_reservation_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Ok(None)
    }

    /// Persist secret-free proof that the exact claim-epoch credential binding
    /// was realized. Implementations fence on the complete claim, verify through
    /// [`verify_credential_realization_receipt`], and make exact retries
    /// idempotent. A stale claim returns [`SettleOutcome::Fenced`].
    async fn record_credential_realization(
        &self,
        _claim: &RunClaim,
        _receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "dispatch backend does not persist credential realization receipts".to_string(),
        ))
    }

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
        _now_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not expose registered-worker run authority".to_string(),
        ))
    }

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

    /// Return durable applied-`Done` facts after `after_sequence`, in ascending
    /// sequence order, capped at `limit`.
    ///
    /// Native durable stores retain these rows as permanent run-id tombstones;
    /// completed ids therefore cannot be re-enqueued after their live dispatch
    /// row is removed (ADR-0060). A transport that does not expose this
    /// server-local projection query fails explicitly.
    async fn completion_events_after(
        &self,
        _after_sequence: u64,
        _limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not expose durable dispatch completion events".to_string(),
        ))
    }

    /// Hold a claim's exact epoch stable across a `ThreadCommit`.
    ///
    /// `Some` means the claim remains authoritative while the guard lives;
    /// `None` means the row is gone or its epoch/owner no longer matches. This is
    /// required: a durable adapter may not degrade to a check-then-commit sequence.
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError>;

    /// Remote-capable checkpoint operations. Native stores normally use
    /// `lock_commit_epoch` around their colocated checkpoint store; transports
    /// override these to execute the guarded operation on the authority server.
    async fn load_stream_checkpoint(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, DispatchError> {
        Err(DispatchError::Rejected(
            "claimed checkpoint transport is unavailable".to_string(),
        ))
    }

    async fn put_stream_checkpoint(
        &self,
        _claim: &RunClaim,
        _checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "claimed checkpoint transport is unavailable".to_string(),
        ))
    }

    async fn delete_stream_checkpoint(
        &self,
        _claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        Err(DispatchError::Rejected(
            "claimed checkpoint transport is unavailable".to_string(),
        ))
    }

    /// Load one claim-authorized, internally consistent committed recovery
    /// prefix. Remote transports override this; local workers already share the
    /// commit reader and fail closed if they accidentally call it.
    async fn load_recovery_snapshot(
        &self,
        _claim: &RunClaim,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>
    {
        Err(DispatchError::Rejected(
            "claimed recovery transport is unavailable".to_string(),
        ))
    }

    /// Manually quarantine expired crashed dispatches at/above `max_attempts`.
    /// Moves rows to `DeadLetter`; automatic services use
    /// [`claim_retry_exhausted`](Self::claim_retry_exhausted), so the Run first
    /// receives committed terminal truth (ADR-0015).
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
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

    /// Current number of dispatches that are claimable at `now_ms`. Native stores
    /// return an exact value; composed/remote backends may return `None` until they
    /// expose an efficient server-side count. This is an operations query only and
    /// never participates in scheduling correctness.
    async fn runnable_depth(&self, _now_ms: u64) -> Result<Option<u64>, DispatchError> {
        Ok(None)
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

    /// The run currently awaiting on a thread, if any. A thread is the stable
    /// addressable unit (a run is one ephemeral execution); this resolves a
    /// thread-addressed delivery to the run awaiting on it.
    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError>;

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
}
