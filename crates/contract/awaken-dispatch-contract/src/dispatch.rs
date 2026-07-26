//! The durable-ingress store ports and their data.
//!
//! Two aggregates, per the run-ingress DDD split:
//!
//! - [`DispatchQueue`] owns *delivery opportunity* — enqueue, single-owner claim,
//!   lease, settle, and lease-expiry recovery. It never owns message payload
//!   truth or a run's outcome; those are the thread and run aggregates, read back
//!   from committed facts.
//! - [`Inbox`] owns the *thread's pending input* — idempotent append of
//!   delivered-but-unconsumed input, frozen once at a safe run boundary.
//!
//! One concrete store implements both (so a wake can freeze pending input inside
//! the same claim transaction), but the traits stay split so neither aggregate
//! reaches into the other's invariants. A blanket [`Dispatch`] bundles them
//! for the worker and host.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_runtime_contract::CredentialRealizationCapabilities;
#[cfg(test)]
use awaken_runtime_contract::resolved::ModelProvisioning;
use awaken_runtime_contract::resume::ResumeResult;
pub use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialBindingError, CandidateFingerprint,
    CredentialRealizationReceipt, CredentialReceiptError, verify_credential_realization_receipt,
};
#[cfg(test)]
use awaken_runtime_contract::{
    CredentialAdmissionError, CredentialRealizationKind, CredentialUsage, PlaintextBoundary,
};
use awaken_worker_contract::{PlacementPolicy, WorkerAssignment, WorkerIdentity, WorkerSnapshot};
use serde::{Deserialize, Serialize};

use crate::run_dispatch::RunDispatch;

pub fn worker_credential_realization_capabilities(
    worker: &WorkerSnapshot,
) -> Result<CredentialRealizationCapabilities, AttemptCredentialBindingError> {
    CredentialRealizationCapabilities::from_manifest_capabilities(&worker.manifest.capabilities)
        .map_err(AttemptCredentialBindingError::InvalidWorkerCapabilities)
}

/// Compile all credential-bearing candidates selected for this Run into exact
/// attempt bindings. Every caller must pass installed capability evidence: an
/// immutable registered Worker manifest or the in-process Worker's composed
/// capabilities. Claim admission never synthesizes capabilities from the request.
pub fn compile_attempt_credential_bindings(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
) -> Result<Vec<AttemptCredentialBinding>, AttemptCredentialBindingError> {
    let candidates = request
        .activation
        .snapshot
        .resolved_spec
        .execution_candidates(request.activation.model_ref_override.as_deref());
    awaken_runtime_contract::compile_candidate_credential_bindings(
        &candidates,
        request.inference_plaintext_holder.as_ref(),
        installed,
        claim_epoch,
        now_unix_ms,
    )
}

/// Read-only eligibility check for a scheduler selecting among multiple rows.
/// Exact claim still calls [`compile_attempt_credential_bindings`] and returns
/// the concrete failure; a broad selector skips a row this Worker cannot admit
/// so it cannot poison unrelated runnable work.
#[must_use]
pub fn can_admit_attempt_credentials(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
) -> bool {
    compile_attempt_credential_bindings(request, installed, claim_epoch, now_unix_ms).is_ok()
}

/// A durable-store failure. Commit-time agent truth uses the commit coordinator's
/// own error; this is only the dispatch queue's own storage failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatch store rejected: {0}")]
    Rejected(String),
}

/// One unit of durable input delivered to a thread, awaiting consumption. Keyed
/// by a stable `message_id` so an at-least-once delivery appends exactly once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInput {
    pub message_id: String,
    pub run_id: RunId,
    pub thread_id: ThreadId,
    /// The awaiting-ticket correlation this input answers. Consumption is keyed to
    /// it: the worker delivers an input only while the committed ticket still
    /// carries the same correlation, so a resume that already committed (and
    /// advanced or cleared the ticket) is never re-applied (ADR-0010).
    pub correlation_id: String,
    /// Earliest delivery time (epoch millis). `None` is deliverable immediately;
    /// a future time schedules the wake — the claim skips it until it is due and
    /// the daemon's poll fires it when the clock reaches it (ADR-0014).
    #[serde(default)]
    pub available_at_ms: Option<u64>,
    /// What this input delivers back into the awaiting run on resume.
    pub result: ResumeResult,
}

/// A lease over one claimed dispatch: the single owner allowed to execute this
/// run until `expires_ms`. An expired lease is reclaimable (recovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub run_id: RunId,
    pub owner: String,
    pub expires_ms: u64,
    /// Monotonic fence token, bumped on every claim of this run (fresh, wake, or
    /// recovery). The holder passes it back on [`settle`](DispatchQueue::settle) so
    /// a *stale* owner — one whose lease lapsed and was re-claimed by another node
    /// under a higher epoch — cannot settle the dispatch out from under the current
    /// owner: the store rejects a settle whose epoch is not the row's current one.
    /// This is the canonical fencing token (Kleppmann): owner strings can collide
    /// or be reused, but a monotone epoch cannot, so the fence is topology- and
    /// owner-name-independent. A row that has never been claimed has epoch 0.
    #[serde(default)]
    pub epoch: u64,
}

/// Durable execution authority for one claimed Run.
///
/// Keeping the run, owner, and fencing epoch together prevents commit, settle,
/// and cancellation code from accidentally mixing fields from different claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunClaim {
    pub run_id: RunId,
    pub owner: String,
    pub epoch: u64,
}

impl From<&Lease> for RunClaim {
    fn from(lease: &Lease) -> Self {
        Self {
            run_id: lease.run_id.clone(),
            owner: lease.owner.clone(),
            epoch: lease.epoch,
        }
    }
}

/// One logical Thread commit plus the exact dispatch authority that admits it.
///
/// Worker identity/incarnation is authenticated by the transport/service edge;
/// the durable coordinator receives only `operation`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedCommitCommand {
    pub claim: RunClaim,
    pub operation: awaken_agent_contract::thread::commit::operation::CommitOperation,
}

/// A claimed, ready-to-run dispatch and the run's undelivered pending input.
/// `pending` is empty for a fresh run and non-empty for a wake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claimed {
    pub request: RunDispatch,
    pub lease: Lease,
    /// Exact credential decisions atomically committed with this lease epoch.
    /// Empty for cancellation and credential-free candidates.
    #[serde(default)]
    pub credential_bindings: Vec<AttemptCredentialBinding>,
    /// A durable cancellation intent recorded before any live signal or terminal
    /// commit. The worker drives this through the same claim/epoch fence as normal
    /// execution, so a crash cannot lose a cancellation or resurrect the run.
    #[serde(default)]
    pub cancellation_requested: bool,
    /// The run's undelivered pending input. The worker decides execute-vs-resume
    /// from committed truth (the awaiting ticket), not from this field, and tells
    /// `settle` which inputs it consumed.
    pub pending: Vec<PendingInput>,
    /// Whether this claim reclaimed an expired running lease. Kept explicit so
    /// operations can distinguish crash recovery from an ordinary await/wake
    /// claim (both advance the fencing epoch).
    #[serde(default)]
    pub recovered: bool,
    /// The sandbox this run is bound to for its lifetime (B-P3, ADR-0021 §6), as an
    /// **opaque** reference — the dispatch aggregate stays neutral (it names no
    /// provisioning type); the fleet serializes a `SandboxHandle` into it and
    /// parses it back on adoption. `None` until the run is placed on a sandbox.
    /// Durable so crash recovery (`reconcile_adoption`) can re-adopt the same
    /// sandbox instead of leaking it.
    pub sandbox: Option<String>,
    /// Worker incarnation selected for this lease epoch. Legacy/local claims omit
    /// it; registered remote claims always persist and return it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<WorkerAssignment>,
}

/// How a claimed attempt resolved. Settled atomically with releasing the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchOutcome {
    /// The run ended; the dispatch is finished and removed.
    Done,
    /// The run awaits external input; keep the dispatch for a later wake.
    Awaiting,
}

/// Whether a [`settle`](DispatchQueue::settle) was applied or fenced off as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettleOutcome {
    /// The caller still held the current lease (its epoch matched the row's); the
    /// settle was applied.
    Applied,
    /// The caller's lease epoch is stale — the run was re-claimed under a higher
    /// epoch (another node recovered the lapsed lease). NOTHING was changed, so the
    /// current owner's in-flight dispatch is untouched. The stale caller has lost
    /// the lease and must abandon the run.
    Fenced,
}

/// One durable observation that a dispatch applied [`DispatchOutcome::Done`].
///
/// This is delivery truth only: the committed run fact remains authoritative for
/// the run's terminal state and cause. `sequence` is a store-assigned cursor;
/// consumers checkpoint it independently and may replay pages idempotently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchCompletion {
    pub sequence: u64,
    pub run_id: RunId,
}

/// Opaque backend guard which keeps a dispatch epoch stable until it is dropped.
/// PostgreSQL stores a row-locking transaction in it; SQLite stores its shared
/// single-process authority mutex guard. The ingress layer needs only the
/// lifetime, never the backend-specific value.
pub struct CommitEpochGuard {
    _held: Box<dyn Send>,
    request: RunDispatch,
    expires_ms: u64,
}

impl CommitEpochGuard {
    #[must_use]
    pub fn new(held: impl Send + 'static, request: RunDispatch, expires_ms: u64) -> Self {
        Self {
            _held: Box::new(held),
            request,
            expires_ms,
        }
    }

    /// The authoritative dispatch payload protected by this exact claim guard.
    /// Recovery uses it to select the Thread without trusting a caller-supplied
    /// Thread id.
    #[must_use]
    pub fn request(&self) -> &RunDispatch {
        &self.request
    }

    /// Whether the guarded claim's lease is live at `now_ms`. The exact expiry
    /// boundary remains live, matching the queue's recovery rule.
    #[must_use]
    pub fn is_live_at(&self, now_ms: u64) -> bool {
        self.expires_ms >= now_ms
    }
}

impl SettleOutcome {
    /// Whether the settle was applied (vs. fenced off as a stale owner's).
    pub fn applied(self) -> bool {
        matches!(self, SettleOutcome::Applied)
    }
}

/// The lifecycle state of a dispatch, for the operational query surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchState {
    /// Fresh, not yet claimed.
    Pending,
    /// Claimed and executing under a lease.
    Leased,
    /// Awaiting on a committed resume ticket.
    Awaiting,
    /// Dead-lettered past its crash-retry budget (ADR-0015).
    DeadLetter,
    /// Superseded by a newer submission on its thread (ADR-0022).
    Superseded,
}

impl DispatchState {
    /// Map the stored status text (the SQL backends' `status` column) to the
    /// public status. Unknown durable vocabulary is rejected rather than turned
    /// into claimable `Pending` work.
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Leased,
            "awaiting" => Self::Awaiting,
            "dead_letter" => Self::DeadLetter,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// An operational view of one dispatch row, for monitoring and maintenance — the
/// `DispatchQueue` query role (ADR-0025). Carries no live handle, just committed
/// queue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub state: DispatchState,
    /// Terminal control has been accepted and is awaiting/under a fenced claim.
    pub cancellation_requested: bool,
    /// Consecutive crash-recoveries without a settle.
    pub attempt_count: u64,
}

/// Dispatch-level options for an accepted run. Defaults to ordinary priority and
/// no caller dedupe key — `enqueue` uses these so existing callers are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitOptions {
    /// Higher runs first among fresh (not-yet-started) work; default 0.
    pub priority: i64,
    /// A caller idempotency key. While a dispatch with this key is live, another
    /// enqueue carrying it is a no-op — dedup beyond the run id (e.g. for an
    /// at-least-once producer). Cleared once the run finishes.
    pub dedupe_key: Option<String>,
    /// Supersede the thread's prior pending/awaiting work: the newest submission
    /// wins, taking the highest epoch; the stale dispatches are marked superseded
    /// and never claimed again (ADR-0022).
    pub supersede: bool,
}

/// Durable run-dispatch queue: activation opportunity, claim, lease, recovery.
#[async_trait]
pub trait DispatchQueue: Send + Sync {
    /// Idempotently record an accepted run at default options. Re-enqueueing the
    /// same run id is a no-op, so an at-least-once submit has an exactly-once
    /// effect per run.
    async fn enqueue(&self, request: RunDispatch) -> Result<(), DispatchError> {
        self.enqueue_with(request, SubmitOptions::default()).await
    }

    /// Record an accepted run with dispatch options (priority, dedupe key). A
    /// dedupe key already live makes this a no-op.
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError>;

    /// Atomically record and claim one newly admitted Run.
    ///
    /// Parent-mediated child creation uses this command so the process-wide
    /// dispatcher cannot claim the new row between a separate enqueue and exact
    /// claim. Existing rows remain idempotent: an existing runnable row may be
    /// claimed under the ordinary exact-claim rules; an already leased or settled
    /// row returns `None`. The returned lease is otherwise identical to
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
    /// dispatch whose lease expired (recovery). Returns `None` when nothing is
    /// runnable, and the run's current pending input in the returned [`Claimed`].
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

    /// Extend the lease on a run this `owner` is executing, so a long run is not
    /// reclaimed by another node's recovery while it is still making progress.
    /// Returns `true` if the lease was renewed (the run is still owned by
    /// `owner`); `false` if it was lost (stolen, settled, or unknown) — the holder
    /// should then stop. This is the multi-node liveness knob (ADR-0019).
    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError>;

    /// Renew, to `now_ms + lease_ms`, the lease on every running dispatch owned by
    /// `owner` that is *within half a lease of expiring* — the daemon's heartbeat
    /// that keeps its in-flight runs from being reclaimed while still executing
    /// (ADR-0024). Returns how many leases were renewed.
    ///
    /// Renewing only near-expiry leases (`lease_until < now_ms + lease_ms/2`), not
    /// every running row on every tick, bounds the write amplification of the
    /// heartbeat: with hundreds of thousands of in-flight runs, a blanket renewal
    /// every few seconds is a storm of no-op-equivalent writes. It stays safe as
    /// long as the heartbeat cadence is under half the lease (the ADR-0024
    /// recommendation), so a lease is always caught within the window before it
    /// expires; a fresh claim, whose lease is a full length out, is skipped until
    /// it approaches expiry.
    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError>;

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

    /// Whether this exact registered Worker incarnation currently owns the live
    /// lease for `run_id`.
    ///
    /// Server-side application capability issuers use this shape because they
    /// authenticate a Worker identity and Run id, but must not trust a
    /// caller-supplied claim epoch.
    async fn worker_owns_run(
        &self,
        _identity: &WorkerIdentity,
        _run_id: &RunId,
        _now_ms: u64,
    ) -> Result<bool, DispatchError> {
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

    /// Dead-letter every *crashed* dispatch — one whose lease expired without a
    /// settle — that has used up its crash-retry budget (`attempt_count >=
    /// max_attempts`). A dead-lettered dispatch is no longer claimed, so a poison
    /// run cannot be reclaimed forever. Returns how many were dead-lettered
    /// (ADR-0015). The crash-retry count increments only on recovery re-claims, so
    /// a normal await/wake never spends the budget.
    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError>;

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

    /// Return a dead-lettered run to the queue at a fresh budget. Returns `true`
    /// if a dead-lettered run with that id was requeued.
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError>;

    /// Durably request cancellation of a pending, awaiting, or running dispatch.
    /// This operation records intent but never removes the row or pending input;
    /// for a running row it also advances the epoch and releases the old lease so
    /// that owner's later commit is fenced. The worker claims it and commits
    /// `Cancelled` before settlement removes delivery state. Repeating it is
    /// idempotent. Returns `None` only for a terminal queue state or unknown run.
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

/// A pending input as stored, with its optimistic-concurrency `revision`. The
/// revision is store-assigned (1 on append, bumped on edit), so it is surfaced
/// on reads — not part of [`PendingInput`], which is the caller's append payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRecord {
    pub input: PendingInput,
    pub revision: u64,
}

/// The result of a revision-guarded pending edit/retract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// The expected revision matched; the change was applied.
    Applied,
    /// The record exists but at a different revision (a concurrent change);
    /// the operation is rejected, fail closed.
    RevisionMismatch,
    /// No pending record with that id (already consumed or never appended).
    NotFound,
}

/// Durable pending-input intake and the thread-message operations over it.
///
/// `append` is the delivery path; `list`/`retract`/`edit` are the thread-message
/// operations surface (run-ingress design: these are NOT `RunIngress` routes).
/// Edit and retract are optimistic: they check the record's `revision` and fail
/// closed on a mismatch, so a concurrent change is never silently overwritten.
/// Records are mutable only before the worker consumes them.
#[async_trait]
pub trait Inbox: Send + Sync {
    /// Idempotently append one pending input at revision 1. Returns `true` if
    /// newly stored, `false` if the `message_id` was already present.
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError>;

    /// The thread's undelivered pending input, in arrival order, with revisions.
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError>;

    /// Retract one pending record if it is still at `expected_revision`.
    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError>;

    /// Replace a pending record's result if it is still at `expected_revision`,
    /// bumping the revision on success.
    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError>;
}

/// Durable cross-thread delivery via a transactional outbox.
///
/// A run on one thread stages a delivery to *another* thread's pending input;
/// the outbox holds it until a relay moves it. The relay is idempotent without a
/// `delivered` flag or two-phase commit: in one store transaction it appends the
/// payload to the target pending input (idempotent by `message_id`) and deletes
/// the outbox row. A crash between the two leaves the outbox row, so the next
/// relay re-appends (a no-op) and deletes — at-least-once with an exactly-once
/// effect (run-ingress design: cross-thread uses outbox + idempotent target
/// append, never 2PC).
#[async_trait]
pub trait Outbox: Send + Sync {
    /// Idempotently stage a cross-thread delivery. The payload carries the target
    /// run/thread it is destined for; the same `message_id` keys both the outbox
    /// row and the eventual pending append.
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError>;

    /// Relay every staged delivery to its target thread's pending input, one
    /// store transaction per message. Returns how many were relayed.
    async fn relay(&self) -> Result<usize, DispatchError>;
}

/// The combined durable-ingress store. One object implements all aggregates so a
/// wake can freeze pending input within a claim and a relay can move outbox to
/// pending in one transaction; the worker and host depend on this bundle, not on
/// a concrete store.
pub trait Dispatch: DispatchQueue + Inbox + Outbox {}

impl<T: DispatchQueue + Inbox + Outbox> Dispatch for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resume::ResumeResult;
    use awaken_runtime_contract::{
        CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialSource,
        CredentialRef, CredentialRefreshAccess, InferenceEndpoint, ModelExposurePolicy,
        PlaintextHolder, SealedCredentialEnvelopeRef, TokenEndpointAuth, TrustDomainRef,
    };
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn a_request() -> RunDispatch {
        let activation = RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: 8,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", "model", "acp:test"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        );
        RunDispatch::new(activation)
    }

    fn holder(boundary: PlaintextBoundary, domain: &str) -> PlaintextHolder {
        PlaintextHolder::new(boundary, domain)
    }

    fn provider_candidate(
        provider: &str,
        model: &str,
        backend: &str,
        credential_id: &str,
        allowed_holder: &PlaintextHolder,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
            ModelBinding::new(provider, model, backend),
            format!("{provider}@1"),
            format!("{provider}-route@1"),
            "workspace-a",
            Some(CredentialAccess::new(
                CredentialRef {
                    id: credential_id.into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(
                    allowed_holder.clone(),
                    ModelExposurePolicy::Forbidden,
                ),
            )),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: model.into(),
            },
        )
    }

    fn credential_access_mut(
        candidate: &mut awaken_runtime_contract::resolved::ResolvedModelCandidate,
    ) -> &mut CredentialAccess {
        let ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } = &mut candidate.provisioning
        else {
            panic!("test candidate must carry provider credential access")
        };
        access
    }

    fn request_with_candidates(
        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate,
        fallbacks: Vec<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
        selected_holder: Option<PlaintextHolder>,
    ) -> RunDispatch {
        let mut request = a_request();
        request.activation.snapshot.resolved_spec.model_binding = primary;
        request.activation.snapshot.resolved_spec.model_candidates = fallbacks;
        request.inference_plaintext_holder = selected_holder;
        request
    }

    fn capabilities(
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> CredentialRealizationCapabilities {
        CredentialRealizationCapabilities {
            holders: BTreeSet::from([selected_holder.clone()]),
            material_sources: BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]),
            realization_kinds: BTreeSet::from([realization]),
            recipient_bound_envelopes: true,
        }
    }

    #[derive(Clone, Copy)]
    enum BindingFixture {
        CredentialFree,
        MissingHolder,
        InvalidUsage,
        NativeWorker,
        NativePlatform,
        AcpWorkload,
        AcpWorker,
        NativeWorkload,
        RemoteWorker,
        HolderUnsupported,
        SourceUnsupported,
        HolderForbidden,
        EnvelopeExpired,
        RefreshRevisionMismatch,
        DuplicateCandidate,
    }

    #[derive(Clone)]
    enum BindingExpected {
        Empty,
        One(CredentialRealizationKind),
        Error(AttemptCredentialBindingError),
    }

    struct BindingRule {
        id: &'static str,
        fixture: BindingFixture,
        claim_epoch: u64,
        expected: BindingExpected,
    }

    /// Cause-effect graph for one selected publication candidate:
    ///
    /// C0 claim epoch > 0
    ///   -> C1 selected candidate carries credential
    ///      -> C2 exact holder requested -> C3 ProviderAdapter usage
    ///      -> C4 backend/holder cell implemented
    ///      -> C5 installed capability + published policy admit the exact tuple
    ///      -> C6 candidate content identity is unique -> E1 frozen binding.
    ///
    /// Credential-free candidates yield E0 (no binding). Every failed cause is
    /// terminal and yields its stable error; the credential contract's own
    /// decision table exhaustively covers the internals of C5.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | B1 | F | - | - | - | - | - | - | invalid epoch |
    /// | B2 | T | F | - | - | - | - | - | empty |
    /// | B3 | T | T | F | - | - | - | - | missing holder |
    /// | B4 | T | T | T | F | - | - | - | invalid usage |
    /// | B5 | T | T | T | T | Native/Worker | T | T | provider adapter |
    /// | B6 | T | T | T | T | Native/Platform | T | T | platform adapter |
    /// | B7 | T | T | T | T | ACP/Workload | T | T | process secret env |
    /// | B8 | T | T | T | T | ACP/Worker | T | T | worker relay |
    /// | B9 | T | T | T | T | Native/Workload | - | - | unsupported |
    /// | B10 | T | T | T | T | Remote/Worker | - | - | unsupported |
    /// | B11 | T | T | T | T | valid | F(holder) | - | unsupported holder |
    /// | B12 | T | T | T | T | valid | F(source) | - | unsupported source |
    /// | B13 | T | T | T | T | valid | F(policy) | - | forbidden holder |
    /// | B14 | T | T | T | T | valid | F(expiry) | - | expired envelope |
    /// | B15 | T | T | T | T | valid | F(revision) | - | refresh mismatch |
    /// | B16 | T | T | T | T | valid | T | F | duplicate candidate |
    #[test]
    fn attempt_binding_cases_are_generated_from_the_decision_table() {
        let rules = [
            BindingRule {
                id: "B1",
                fixture: BindingFixture::NativeWorker,
                claim_epoch: 0,
                expected: BindingExpected::Error(AttemptCredentialBindingError::InvalidClaimEpoch),
            },
            BindingRule {
                id: "B2",
                fixture: BindingFixture::CredentialFree,
                claim_epoch: 3,
                expected: BindingExpected::Empty,
            },
            BindingRule {
                id: "B3",
                fixture: BindingFixture::MissingHolder,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::MissingPlaintextHolder,
                ),
            },
            BindingRule {
                id: "B4",
                fixture: BindingFixture::InvalidUsage,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::InvalidCredentialUsage,
                ),
            },
            BindingRule {
                id: "B5",
                fixture: BindingFixture::NativeWorker,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerProviderAdapter),
            },
            BindingRule {
                id: "B6",
                fixture: BindingFixture::NativePlatform,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::PlatformProviderAdapter),
            },
            BindingRule {
                id: "B7",
                fixture: BindingFixture::AcpWorkload,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::ProcessSecretEnvironment),
            },
            BindingRule {
                id: "B8",
                fixture: BindingFixture::AcpWorker,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerRelay),
            },
            BindingRule {
                id: "B9",
                fixture: BindingFixture::NativeWorkload,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::UnsupportedRealization {
                        boundary: PlaintextBoundary::Workload,
                        backend: "genai".into(),
                    },
                ),
            },
            BindingRule {
                id: "B10",
                fixture: BindingFixture::RemoteWorker,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::UnsupportedRealization {
                        boundary: PlaintextBoundary::Worker,
                        backend: "a2a:https://agent.invalid".into(),
                    },
                ),
            },
            BindingRule {
                id: "B11",
                fixture: BindingFixture::HolderUnsupported,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::HolderUnsupported,
                )),
            },
            BindingRule {
                id: "B12",
                fixture: BindingFixture::SourceUnsupported,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::MaterialSourceUnsupported,
                )),
            },
            BindingRule {
                id: "B13",
                fixture: BindingFixture::HolderForbidden,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::HolderNotAllowed,
                )),
            },
            BindingRule {
                id: "B14",
                fixture: BindingFixture::EnvelopeExpired,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::EnvelopeExpired,
                )),
            },
            BindingRule {
                id: "B15",
                fixture: BindingFixture::RefreshRevisionMismatch,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::CredentialRevisionMismatch,
                )),
            },
            BindingRule {
                id: "B16",
                fixture: BindingFixture::DuplicateCandidate,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::DuplicateCandidate),
            },
        ];

        for rule in rules {
            let worker_holder = holder(PlaintextBoundary::Worker, "worker-a");
            let workload_holder = holder(PlaintextBoundary::Workload, "workload-a");
            let platform_holder = holder(PlaintextBoundary::Platform, "platform-a");
            let (backend, selected_holder, realization) = match rule.fixture {
                BindingFixture::NativePlatform => (
                    "genai",
                    platform_holder.clone(),
                    CredentialRealizationKind::PlatformProviderAdapter,
                ),
                BindingFixture::AcpWorkload | BindingFixture::NativeWorkload => (
                    if matches!(rule.fixture, BindingFixture::AcpWorkload) {
                        "acp:claude"
                    } else {
                        "genai"
                    },
                    workload_holder.clone(),
                    CredentialRealizationKind::ProcessSecretEnvironment,
                ),
                BindingFixture::AcpWorker => (
                    "acp:claude",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerRelay,
                ),
                BindingFixture::RemoteWorker => (
                    "a2a:https://agent.invalid",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerProviderAdapter,
                ),
                _ => (
                    "genai",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerProviderAdapter,
                ),
            };
            let mut candidate = provider_candidate(
                "provider-a",
                "model-a",
                backend,
                "credential-a",
                &selected_holder,
            );
            if matches!(rule.fixture, BindingFixture::InvalidUsage) {
                credential_access_mut(&mut candidate).usage = CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                };
            }
            if matches!(rule.fixture, BindingFixture::HolderForbidden) {
                credential_access_mut(&mut candidate).policy = CredentialExecutionPolicy::exact(
                    holder(PlaintextBoundary::Worker, "worker-b"),
                    ModelExposurePolicy::Forbidden,
                );
            }
            if matches!(rule.fixture, BindingFixture::EnvelopeExpired) {
                let access = credential_access_mut(&mut candidate).clone().with_envelope(
                    CredentialEnvelope::SealedForWorker {
                        envelope_ref: SealedCredentialEnvelopeRef {
                            id: "envelope-a".into(),
                            payload_fingerprint: "sha256:payload".into(),
                        },
                        recipient: TrustDomainRef(selected_holder.trust_domain.0.clone()),
                        expires_at_unix_ms: 9,
                    },
                );
                *credential_access_mut(&mut candidate) = access;
            }
            if matches!(rule.fixture, BindingFixture::RefreshRevisionMismatch) {
                let access = credential_access_mut(&mut candidate).clone().with_refresh(
                    CredentialRefreshAccess::new(
                        8,
                        "https://auth.invalid/token".into(),
                        "client-a".into(),
                        TokenEndpointAuth::None,
                        None,
                        "refresh-a".into(),
                        "access-a".into(),
                        None,
                        None,
                    ),
                );
                *credential_access_mut(&mut candidate) = access;
            }
            let primary = if matches!(rule.fixture, BindingFixture::CredentialFree) {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                    "host", "model-a", "native",
                ))
            } else {
                candidate.clone()
            };
            let fallbacks = if matches!(rule.fixture, BindingFixture::DuplicateCandidate) {
                vec![candidate]
            } else {
                Vec::new()
            };
            let requested_holder = if matches!(
                rule.fixture,
                BindingFixture::CredentialFree | BindingFixture::MissingHolder
            ) {
                None
            } else {
                Some(selected_holder.clone())
            };
            let request = request_with_candidates(primary, fallbacks, requested_holder);
            let mut installed = capabilities(&selected_holder, realization);
            if matches!(rule.fixture, BindingFixture::HolderUnsupported) {
                installed.holders.clear();
            }
            if matches!(rule.fixture, BindingFixture::SourceUnsupported) {
                installed.material_sources.clear();
            }
            let actual =
                compile_attempt_credential_bindings(&request, &installed, rule.claim_epoch, 10);
            match rule.expected {
                BindingExpected::Empty => assert_eq!(actual, Ok(Vec::new()), "{}", rule.id),
                BindingExpected::One(kind) => {
                    let bindings = actual.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
                    assert_eq!(bindings.len(), 1, "{}", rule.id);
                    assert_eq!(bindings[0].selected_realization_kind, kind, "{}", rule.id);
                    assert_eq!(bindings[0].claim_epoch, rule.claim_epoch, "{}", rule.id);
                    assert_eq!(bindings[0].credential.id, "credential-a", "{}", rule.id);
                    assert!(
                        bindings[0].candidate_fingerprint.0.starts_with("sha256:"),
                        "{}",
                        rule.id
                    );
                }
                BindingExpected::Error(expected) => {
                    assert_eq!(actual, Err(expected), "{}", rule.id);
                }
            }
        }
    }

    /// Candidate-selection cause graph:
    ///
    /// C1 nonblank override -> select every published route for that model.
    /// !C1 -> retain the complete ordered primary/fallback pool.
    /// Credential-free selected candidates contribute no binding, while every
    /// credential-bearing candidate contributes exactly one distinct binding.
    ///
    /// | Rule | override | selected publication | Result |
    /// |---|---|---|---|
    /// | S1 | absent | primary + both fallbacks | three ordered bindings |
    /// | S2 | model-b | both model-b routes only | two ordered bindings |
    /// | S3 | model-c | credential-free host only | empty |
    #[test]
    fn binding_set_preserves_published_fallbacks_and_model_override_scope() {
        let selected = holder(PlaintextBoundary::Worker, "worker-a");
        let primary =
            provider_candidate("provider-a", "model-a", "genai", "credential-a", &selected);
        let route_b1 = provider_candidate(
            "provider-b1",
            "model-b",
            "genai",
            "credential-b1",
            &selected,
        );
        let route_b2 = provider_candidate(
            "provider-b2",
            "model-b",
            "genai",
            "credential-b2",
            &selected,
        );
        let host = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "model-c", "native"),
        );
        let installed = capabilities(&selected, CredentialRealizationKind::WorkerProviderAdapter);
        let base = request_with_candidates(primary, vec![route_b1, route_b2, host], Some(selected));

        let all = compile_attempt_credential_bindings(&base, &installed, 11, 10)
            .expect("S1 complete pool binds");
        assert_eq!(
            all.iter()
                .map(|binding| binding.credential.id.as_str())
                .collect::<Vec<_>>(),
            vec!["credential-a", "credential-b1", "credential-b2"]
        );
        assert_eq!(
            all.iter()
                .map(|binding| &binding.candidate_fingerprint)
                .collect::<BTreeSet<_>>()
                .len(),
            3,
            "each publication route has a distinct content identity"
        );

        let mut model_b = base.clone();
        model_b.activation.model_ref_override = Some("model-b".into());
        let selected_b = compile_attempt_credential_bindings(&model_b, &installed, 12, 10)
            .expect("S2 same-model routes bind");
        assert_eq!(
            selected_b
                .iter()
                .map(|binding| binding.credential.id.as_str())
                .collect::<Vec<_>>(),
            vec!["credential-b1", "credential-b2"]
        );

        let mut model_c = base;
        model_c.activation.model_ref_override = Some("model-c".into());
        assert_eq!(
            compile_attempt_credential_bindings(&model_c, &installed, 13, 10),
            Ok(Vec::new()),
            "S3 selected host candidate needs no credential binding"
        );
    }

    /// Receipt cause-effect graph:
    ///
    /// exact binding identity + exact planned/actual mechanism + intact content
    /// fingerprint -> verified receipt. Any mismatch fails closed before the
    /// receipt can become durable evidence.
    ///
    /// | Rule | binding fields | mechanism | fingerprint | Result |
    /// |---|---|---|---|---|
    /// | P1 | exact | exact | exact | verified |
    /// | P2 | mismatch | exact | exact | binding mismatch |
    /// | P3 | exact | mismatch | - | mechanism mismatch |
    /// | P4 | exact | exact | mismatch | fingerprint mismatch |
    #[test]
    fn realization_receipt_cases_are_generated_from_the_decision_table() {
        let binding = AttemptCredentialBinding {
            candidate_fingerprint: CandidateFingerprint("sha256:candidate-a".into()),
            credential: CredentialRef {
                id: "credential-a".into(),
                revision: 7,
            },
            selected_plaintext_holder: holder(PlaintextBoundary::Worker, "worker-a"),
            selected_realization_kind: CredentialRealizationKind::WorkerProviderAdapter,
            claim_epoch: 9,
        };
        let valid = CredentialRealizationReceipt::new(
            &binding,
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .expect("P1 exact receipt");
        assert_eq!(valid.verify(&binding), Ok(()));

        let mismatched_bindings = [
            AttemptCredentialBinding {
                candidate_fingerprint: CandidateFingerprint("sha256:candidate-b".into()),
                ..binding.clone()
            },
            AttemptCredentialBinding {
                credential: CredentialRef {
                    id: "credential-b".into(),
                    revision: 7,
                },
                ..binding.clone()
            },
            AttemptCredentialBinding {
                selected_plaintext_holder: holder(PlaintextBoundary::Worker, "worker-b"),
                ..binding.clone()
            },
            AttemptCredentialBinding {
                claim_epoch: 10,
                ..binding.clone()
            },
        ];
        for mismatched in mismatched_bindings {
            assert_eq!(
                valid.verify(&mismatched),
                Err(CredentialReceiptError::BindingMismatch),
                "P2 every binding coordinate is fenced"
            );
        }

        let mut wrong_mechanism = valid.clone();
        wrong_mechanism.actual_realization_kind = CredentialRealizationKind::WorkerRelay;
        assert_eq!(
            wrong_mechanism.verify(&binding),
            Err(CredentialReceiptError::MechanismMismatch),
            "P3 actual mechanism cannot differ from admission"
        );

        let mut tampered = valid.clone();
        tampered.receipt_fingerprint = "sha256:tampered".into();
        assert_eq!(
            tampered.verify(&binding),
            Err(CredentialReceiptError::FingerprintMismatch),
            "P4 receipt content is tamper-evident"
        );

        let wire = serde_json::to_string(&(binding, valid)).expect("receipt wire serializes");
        assert!(!wire.contains("secret-material"));
        assert!(!wire.contains("api_key"));
    }

    /// The SQL backends' `status` column maps to the public enum, and any value
    /// outside the known set is rejected rather than becoming runnable.
    #[test]
    fn dispatch_status_from_db_maps_known_values_and_rejects_unknowns() {
        assert_eq!(
            DispatchState::from_db("running"),
            Some(DispatchState::Leased)
        );
        assert_eq!(
            DispatchState::from_db("dead_letter"),
            Some(DispatchState::DeadLetter)
        );
        assert_eq!(
            DispatchState::from_db("superseded"),
            Some(DispatchState::Superseded)
        );
        assert_eq!(
            DispatchState::from_db("pending"),
            Some(DispatchState::Pending)
        );
        assert_eq!(DispatchState::from_db("Running"), None); // case-sensitive
        assert_eq!(DispatchState::from_db("bogus"), None);
        assert_eq!(DispatchState::from_db(""), None);
    }

    /// The neutral submit default every existing caller inherits: ordinary priority,
    /// no dedupe key, no supersede — so `enqueue` is behavior-unchanged.
    #[test]
    fn submit_options_default_is_ordinary_priority_no_dedupe_no_supersede() {
        let o = SubmitOptions::default();
        assert_eq!(o.priority, 0);
        assert!(o.dedupe_key.is_none());
        assert!(!o.supersede);
    }

    fn pending() -> PendingInput {
        PendingInput {
            message_id: "m1".into(),
            run_id: RunId("run-1".into()),
            thread_id: ThreadId("thrd-1".into()),
            correlation_id: "corr-1".into(),
            available_at_ms: Some(1_234),
            result: ResumeResult::allow(),
        }
    }

    /// A pending-input row round-trips through JSON unchanged — the durable delivery
    /// payload the inbox persists.
    #[test]
    fn pending_input_round_trips_through_serde() {
        let p = pending();
        let json = serde_json::to_string(&p).expect("serializes");
        let back: PendingInput = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, p);
    }

    /// ADR-0014 added `available_at_ms` with `#[serde(default)]`: a row written
    /// before it (no such key) must deserialize as `None`, not fail — otherwise a
    /// pre-existing pending input would be dropped (a no-data-loss invariant). Built
    /// by stripping the key from a real row, so it never hardcodes `ResumeResult`'s
    /// wire shape.
    #[test]
    fn a_pending_row_without_available_at_ms_loads_as_none() {
        let mut v = serde_json::to_value(pending()).expect("to value");
        v.as_object_mut().expect("object").remove("available_at_ms");
        let back: PendingInput = serde_json::from_value(v).expect("legacy row loads");
        assert_eq!(back.available_at_ms, None);
    }

    /// The claim/settle wire payloads round-trip through JSON — the lease a worker
    /// holds and the outcome it settles with must survive the HTTP dispatch
    /// transport (a cross-node worker claims/settles over the wire, not the DB).
    #[test]
    fn lease_and_outcome_round_trip_through_serde() {
        let lease = Lease {
            run_id: RunId("run-1".into()),
            owner: "host-7-42".into(),
            expires_ms: 9_999,
            epoch: 3,
        };
        let back: Lease = serde_json::from_str(&serde_json::to_string(&lease).expect("serializes"))
            .expect("deserializes");
        assert_eq!(back, lease);

        // A pre-fence lease row (no `epoch` key) loads with epoch 0 — the neutral
        // value a never-claimed row carries, so an old persisted/wire lease is not
        // rejected (the fence only fires when a HIGHER epoch supersedes it).
        let mut v = serde_json::to_value(&lease).expect("to value");
        v.as_object_mut().expect("object").remove("epoch");
        let legacy: Lease = serde_json::from_value(v).expect("legacy lease loads");
        assert_eq!(legacy.epoch, 0);

        for outcome in [DispatchOutcome::Done, DispatchOutcome::Awaiting] {
            let back: DispatchOutcome =
                serde_json::from_str(&serde_json::to_string(&outcome).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, outcome);
        }
    }

    /// `applied()` is the single predicate the worker branches on after a settle: it
    /// is true only for `Applied`, so a `Fenced` (stale-owner) settle never reads as
    /// success and the stale owner abandons the run.
    #[test]
    fn settle_outcome_applied_is_true_only_when_applied() {
        assert!(SettleOutcome::Applied.applied());
        assert!(!SettleOutcome::Fenced.applied());
    }

    /// `SettleOutcome` is a settle *response*; a cross-node worker settles over the
    /// wire, so both variants must survive a JSON round-trip intact.
    #[test]
    fn settle_outcome_round_trips_through_serde() {
        for o in [SettleOutcome::Applied, SettleOutcome::Fenced] {
            let back: SettleOutcome =
                serde_json::from_str(&serde_json::to_string(&o).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, o);
        }
    }

    /// The full claim payload — request + lease + pending + a bound sandbox ref —
    /// round-trips through JSON, since a cross-node worker receives `Claimed` over the
    /// dispatch transport, not out of the DB.
    #[test]
    fn claimed_round_trips_through_serde_including_sandbox_binding() {
        let claimed = Claimed {
            request: a_request(),
            lease: Lease {
                run_id: RunId("run-1".into()),
                owner: "host-7".into(),
                expires_ms: 5_000,
                epoch: 2,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: true,
            pending: vec![pending()],
            recovered: true,
            sandbox: Some("sbx-opaque-ref".into()),
            assignment: None,
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&claimed).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back, claimed);
        assert_eq!(back.sandbox.as_deref(), Some("sbx-opaque-ref"));
        assert!(back.cancellation_requested);

        // An unplaced run (no sandbox yet) round-trips with `sandbox: None`.
        let unplaced = Claimed {
            sandbox: None,
            ..claimed
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&unplaced).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back.sandbox, None);
    }

    /// A `DispatchQueue` that records the options its `enqueue_with` was called with,
    /// to prove the default `enqueue` delegates at `SubmitOptions::default()` and that
    /// `bind_sandbox` defaults to a no-op `Ok(())`. Every other method fails with an
    /// explicit typed error, so extending the test cannot introduce a panic path.
    #[derive(Default)]
    struct CapturingQueue {
        last_options: Mutex<Option<SubmitOptions>>,
    }

    impl CapturingQueue {
        fn unsupported<T>() -> Result<T, DispatchError> {
            Err(DispatchError::Rejected(
                "operation is outside the CapturingQueue test scope".to_string(),
            ))
        }
    }

    #[async_trait]
    impl DispatchQueue for CapturingQueue {
        async fn lock_commit_epoch(
            &self,
            _claim: &RunClaim,
        ) -> Result<Option<CommitEpochGuard>, DispatchError> {
            Self::unsupported()
        }

        async fn enqueue_with(
            &self,
            _request: RunDispatch,
            options: SubmitOptions,
        ) -> Result<(), DispatchError> {
            *self.last_options.lock().unwrap() = Some(options);
            Ok(())
        }
        async fn claim_new_run(
            &self,
            _request: RunDispatch,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn deliver_and_claim(
            &self,
            _input: PendingInput,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn claim(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }

        async fn claim_run(
            &self,
            _run_id: &RunId,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn renew_lease(
            &self,
            _run_id: &RunId,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<bool, DispatchError> {
            Self::unsupported()
        }
        async fn renew_owned_leases(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn settle(
            &self,
            _run_id: &RunId,
            _epoch: u64,
            _outcome: DispatchOutcome,
            _consumed: &[String],
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }
        async fn reap(&self, _max_attempts: u64, _now_ms: u64) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
            Self::unsupported()
        }
        async fn requeue(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
            Self::unsupported()
        }
        async fn cancel(&self, _run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
            Self::unsupported()
        }
        async fn awaiting_run(
            &self,
            _thread_id: &ThreadId,
        ) -> Result<Option<RunId>, DispatchError> {
            Self::unsupported()
        }
        async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn purge_dead_letters_before(&self, _cutoff_ms: u64) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
            Self::unsupported()
        }
        async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
            Self::unsupported()
        }
    }

    #[tokio::test]
    async fn enqueue_delegates_to_enqueue_with_at_default_options() {
        let q = CapturingQueue::default();
        q.enqueue(a_request()).await.expect("enqueue ok");
        assert_eq!(
            q.last_options.lock().unwrap().clone(),
            Some(SubmitOptions::default()),
            "the convenience enqueue must submit at default priority/dedupe/supersede"
        );
    }

    #[tokio::test]
    async fn bind_sandbox_defaults_to_fail_closed() {
        // Backends that do not persist the binding cannot pretend the write applied.
        let q = CapturingQueue::default();
        assert_eq!(
            q.bind_sandbox(
                &RunClaim {
                    run_id: RunId("run-1".into()),
                    owner: "worker".into(),
                    epoch: 1,
                },
                "sbx-ref"
            )
            .await
            .unwrap(),
            SettleOutcome::Fenced
        );
    }
}
