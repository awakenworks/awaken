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
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_runtime_contract::CredentialRealizationCapabilities;
use awaken_runtime_contract::resume::ResumeResult;
pub use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialBindingError, CandidateFingerprint,
    CredentialRealizationReceipt, CredentialReceiptError, verify_credential_realization_receipt,
};
#[cfg(test)]
use awaken_runtime_contract::{
    CredentialAdmissionError, CredentialRealizationKind, CredentialUsage, PlaintextBoundary,
};
use awaken_session_contract::{SessionRunActivityAdmission, SessionRunActivityAdmissionMode};
use awaken_worker_contract::{PlacementPolicy, WorkerAssignment, WorkerIdentity, WorkerSnapshot};
use serde::{Deserialize, Serialize};

use crate::run_dispatch::RunDispatch;

include!("commit_epoch.rs");

mod credential_admission;
pub use credential_admission::{
    DispatchCredentialAdmissionError, can_admit_attempt_credentials,
    compile_attempt_credential_bindings, worker_credential_realization_capabilities,
};

/// A durable-store failure. Commit-time agent truth uses the commit coordinator's
/// own error; this is only the dispatch queue's own storage failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatch store rejected: {0}")]
    Rejected(String),
}

/// Failure at a committed Run boundary that must complete before delivery
/// state can be removed or returned to Awaiting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("dispatch settlement observer failed: {0}")]
pub struct DispatchSettlementError(pub String);

/// Fallible pre-settlement effect over trusted dispatch/claim coordinates and
/// committed Run truth. An error retains the dispatch row as retry evidence.
#[async_trait]
pub trait DispatchSettlementObserver: Send + Sync {
    /// Whether this observer's transport holds the authoritative reservation
    /// phase guard across its Session root CAS. Local observers return false so
    /// the Worker holds the colocated queue guard; remote authenticated adapters
    /// return true because the guard exists only at the Coordinator endpoint.
    fn owns_session_run_reservation_fence(&self) -> bool {
        false
    }

    /// Repair the exact Session activity admission before an expired Reserved
    /// intent is published to ordinary Pending. Implementations must derive a
    /// stable operation from the canonical Run identity.
    async fn admit_session_run_activity(
        &self,
        _dispatch: &RunDispatch,
        _claim: &RunClaim,
        _mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        Err(DispatchSettlementError(
            "Session Run activity admission is not installed".to_string(),
        ))
    }

    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError>;
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
    /// advanced or cleared the ticket) is never re-applied (ADR-0010). A fresh
    /// continuation instead carries its exact future `run_id` and an empty
    /// correlation; a generic idle-Thread input leaves both fields empty
    /// (ADR-0021).
    pub correlation_id: String,
    /// Earliest delivery time (epoch millis). `None` is deliverable immediately;
    /// a future time schedules the wake — the claim skips it until it is due and
    /// the daemon's poll fires it when the clock reaches it (ADR-0014).
    #[serde(default)]
    pub available_at_ms: Option<u64>,
    /// What this input delivers back into the awaiting run on resume.
    pub result: ResumeResult,
    /// Stable committed-context Messages accepted with this input. A Session
    /// tool reply uses this only for its immediately accompanying System
    /// Message; the Worker copies the frozen values into `ResumeCommand` and
    /// Runtime commits them before the correlated tool result in one delta.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_messages: Vec<awaken_agent_contract::agent::message::Message>,
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
/// `pending` is normally empty for a fresh Run, but contains an immediate
/// Run-bound input when an Outbox message atomically admitted that continuation.
/// It is non-empty for an ordinary wake.
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
    /// This lease owns an expired Session Run reservation. The Worker must
    /// recover the root activity receipt and atomically resolve the reservation
    /// before it may enter the Run executor.
    #[serde(default)]
    pub session_activity_admission_required: bool,
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
    /// Logical Thread and optional Session routing affinity retained from the
    /// accepted dispatch. A Session root is self-affine; a coordinated child is
    /// affine to its parent. Historical rows may omit both. Admission uses the
    /// pair only to enforce the Thread/archive limit, never as a second
    /// relationship or archive source of truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_thread_id: Option<ThreadId>,
    /// Canonical identity of the accepted dispatch. Historical tombstones that
    /// predate collision detection retain `None` and therefore cannot authorize
    /// a caller-owned Run-id replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
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
    /// Complete Session Run intent awaiting its root activity admission. It is
    /// invisible to ordinary claims until its reservation deadline expires.
    Reserved,
    /// An exclusive recovery claim is repairing the Reserved intent's Session
    /// activity admission. The Run executor has not started.
    ReservationLeased,
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
            "reserved" => Self::Reserved,
            "reservation_running" => Self::ReservationLeased,
            "pending" => Self::Pending,
            "running" => Self::Leased,
            "awaiting" => Self::Awaiting,
            "dead_letter" => Self::DeadLetter,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// Whether a prior Session dispatch has crossed a boundary at which queue
/// replacement cannot strand an open Session activity.
#[must_use]
pub const fn session_run_replacement_candidate_is_safe(state: DispatchState) -> bool {
    matches!(state, DispatchState::Awaiting | DispatchState::Superseded)
}

#[cfg(kani)]
mod session_replacement_proofs {
    use super::{DispatchState, session_run_replacement_candidate_is_safe};

    #[kani::proof]
    fn only_awaiting_or_already_superseded_is_replacement_safe() {
        let tag: u8 = kani::any();
        kani::assume(tag < 7);
        let state = match tag {
            0 => DispatchState::Reserved,
            1 => DispatchState::ReservationLeased,
            2 => DispatchState::Pending,
            3 => DispatchState::Leased,
            4 => DispatchState::Awaiting,
            5 => DispatchState::DeadLetter,
            _ => DispatchState::Superseded,
        };
        assert_eq!(
            session_run_replacement_candidate_is_safe(state),
            matches!(state, DispatchState::Awaiting | DispatchState::Superseded)
        );
    }
}

/// An operational view of one dispatch row, for monitoring and maintenance — the
/// `DispatchQueue` query role (ADR-0025). Carries no live handle, just committed
/// queue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    /// Existing Session routing affinity. `None` denotes an ordinary root Run,
    /// self-affinity denotes a Session root, and a different Thread denotes a
    /// coordinated child. Operational cleanup can use this projection to find
    /// every child without a second relationship registry.
    pub session_thread_id: Option<ThreadId>,
    /// Durable Session activity coordinate currently carried by this row. It
    /// lets the Session root atomically transfer a committed continuation away
    /// from the finishing attempt before the outbox rotates the row.
    pub session_activity_epoch: Option<u64>,
    /// Expiry of the exclusive admission owner while `state == Reserved`.
    /// Other states report `None`.
    pub reservation_deadline_ms: Option<u64>,
    pub state: DispatchState,
    /// Terminal control has been accepted and is awaiting/under a fenced claim.
    pub cancellation_requested: bool,
    /// Consecutive crash-recoveries without a settle.
    pub attempt_count: u64,
    /// Whether an opaque Sandbox handle has been durably bound. The monitoring
    /// view deliberately exposes no provider-specific handle material.
    pub sandbox_bound: bool,
}

/// Claim-fenced resolution of one expired Session Run reservation. This mutates
/// the existing dispatch row only; it is not a second command or activity log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReservationResolution {
    /// The canonical Session activity receipt committed. Bind it to the row and
    /// publish Pending; a later ordinary claim remains the only executor entry.
    Admitted { session_activity_epoch: u64 },
    /// Session policy definitively rejected the still-unstarted intent. Remove
    /// the reservation without writing a Run completion tombstone.
    Rejected,
    /// Admission authority was unavailable. Return the exact claim to Reserved
    /// for this store-clock TTL, retaining the same Run identity.
    Retry { reservation_ttl_ms: u64 },
}

/// Closed evidence returned when a caller binds a committed Session activity to
/// its one durable Run reservation. Callers use this instead of guessing whether
/// a false CAS result means replay, recovery ownership, completion, or absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRunReservationActivation {
    Activated,
    AlreadyActivated { session_activity_epoch: u64 },
    RecoveryClaimed,
    Completed,
    MissingOrRejected,
    Conflict,
}

/// Closed evidence returned before a caller opens a Session activity. This
/// prevents a completion or conflicting replay from creating an epoch that
/// would then require compensating recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReservationOutcome {
    Reserved,
    AlreadyReserved,
    RecoveryClaimed,
    AlreadyActivated { session_activity_epoch: u64 },
    Completed,
    Conflict,
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

/// Trusted policy and committed archive evidence for one Session-child
/// admission.
///
/// `archived_threads` is a caller-supplied projection of authoritative committed
/// [`ThreadDisposition`](awaken_agent_contract::ThreadDisposition) state. The
/// queue never persists archive flags. It combines this evidence with its live
/// dispatches and completion tombstones under one admission transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChildAdmission {
    pub max_unarchived_threads: usize,
    pub archived_threads: Vec<ThreadId>,
    /// Derived child Threads that share the Session partition but do not belong
    /// to this Managed capacity class (currently publication-pinned advisor
    /// consultations). This is transient authoritative evidence, not a stored
    /// flag or relationship registry.
    pub capacity_exempt_threads: Vec<ThreadId>,
}

impl SessionChildAdmission {
    #[must_use]
    pub fn new(max_unarchived_threads: usize, archived_threads: Vec<ThreadId>) -> Self {
        Self {
            max_unarchived_threads,
            archived_threads,
            capacity_exempt_threads: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_capacity_exempt_threads(mut self, threads: Vec<ThreadId>) -> Self {
        self.capacity_exempt_threads = threads;
        self
    }
}

/// Admission authority for a fresh Run activated by one Outbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationAdmission {
    /// A root Run. Ordinary roots carry no Session affinity; a Session root
    /// carries the existing canonical self-affinity (`session_thread_id` equals
    /// its own Thread). A foreign parent affinity requires [`SessionChild`](Self::SessionChild).
    Root,
    /// Existing/new Session-child Thread under trusted archive evidence.
    SessionChild(SessionChildAdmission),
}

include!("dispatch/queue.rs");

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

    /// Atomically stage one bound resume input to a committed-Awaiting,
    /// Session-affine Run and rotate that dispatch's Session activity
    /// settlement coordinate.
    ///
    /// The existing dispatch row remains the execution and routing authority:
    /// implementations validate its exact Run/Thread/Session affinity and the
    /// caller's expected prior activity coordinate, reject a competing resume
    /// for the same awaiting correlation, then update `session_activity_epoch`
    /// and insert the Outbox payload in one backend transaction. A `None` prior
    /// coordinate is reserved for the foreground-to-durable handoff whose newly
    /// enqueued row already carries `session_activity_epoch`. The queue row may still be
    /// Leased after the Worker committed its Awaiting ticket but before it
    /// settles; accepting that phase closes the commit/settle discovery race.
    /// An exact replay returns `false`; it never rewrites the row or creates
    /// another delivery.
    async fn stage_session_resume(
        &self,
        _input: PendingInput,
        _session_thread_id: &ThreadId,
        _prior_session_activity_epoch: Option<u64>,
        _session_activity_epoch: u64,
    ) -> Result<bool, DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support Session resume staging".to_string(),
        ))
    }

    /// Relay every staged delivery to its target thread's pending input, one
    /// store transaction per message. Returns how many were relayed.
    async fn relay(&self) -> Result<usize, DispatchError>;

    /// Atomically deliver one immediate cross-Thread input bound to the exact
    /// fresh Run that will consume it, and enqueue that Run under explicit
    /// root/Session-child admission authority.
    ///
    /// This is the idle-target continuation twin of [`relay`](Self::relay): the
    /// outbox still owns cross-Thread delivery and the ordinary dispatch row
    /// still owns activation. The single transaction closes the otherwise fatal
    /// window where a separately staged input can be relayed without its Run (or
    /// vice versa). The caller submits the one canonical `PendingInput` value,
    /// whose `run_id` must equal the dispatch Run; this command performs the
    /// target append and dispatch insertion in the same backend transaction.
    /// Exact canonical Run replay is a no-op and never resurrects consumed input.
    async fn relay_and_enqueue(
        &self,
        _input: PendingInput,
        _request: RunDispatch,
        _admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        Err(DispatchError::Rejected(
            "backend does not support atomic outbox continuation admission".to_string(),
        ))
    }
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

    #[test]
    fn session_replacement_safety_partition_is_exhaustive() {
        // Cause/effect graph: each closed DispatchState either has crossed the
        // settled Awaiting boundary/already been superseded, or may still own
        // execution/admission activity. Effect: only the first partition is
        // replaceable. This table must name every variant so adding a phase
        // forces an explicit policy decision.
        let cases = [
            (DispatchState::Reserved, false),
            (DispatchState::ReservationLeased, false),
            (DispatchState::Pending, false),
            (DispatchState::Leased, false),
            (DispatchState::Awaiting, true),
            (DispatchState::DeadLetter, false),
            (DispatchState::Superseded, true),
        ];
        for (state, expected) in cases {
            assert_eq!(
                session_run_replacement_candidate_is_safe(state),
                expected,
                "closed replacement policy for {state:?}"
            );
        }
    }
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
        candidate_with_access(
            provider,
            model,
            backend,
            CredentialAccess::new(
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
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                provider,
            )),
        )
    }

    fn candidate_with_access(
        provider: &str,
        model: &str,
        backend: &str,
        access: CredentialAccess,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        let binding = ModelBinding::new(
            provider,
            if backend.starts_with("a2a:") {
                ""
            } else {
                model
            },
            backend,
        );
        if backend.starts_with("a2a:") {
            return awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
                binding,
                "workspace-a",
                Some(access),
                "sha256:test-agent-card",
            )
            .expect("coherent dispatch remote candidate");
        }
        let endpoint = InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://provider.invalid/v1".into(),
            upstream_model: model.into(),
            processing_placement: None,
        };
        let result = if backend.starts_with("acp:") {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                binding,
                format!("{provider}@1"),
                format!("{provider}-route@1"),
                "workspace-a",
                Some(access),
                endpoint,
                awaken_runtime_contract::resolved::AcpExecutionProfile {
                    capability_adapter_version: "test".into(),
                    capability_fingerprint: "sha256:test-capability".into(),
                    session_configuration: Default::default(),
                },
            )
        } else {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
                binding,
                format!("{provider}@1"),
                format!("{provider}-route@1"),
                "workspace-a",
                Some(access),
                endpoint,
            )
        };
        result.expect("coherent dispatch provider candidate")
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
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        }
    }

    fn acp_mcp_capabilities(
        selected_holder: &PlaintextHolder,
        backend_ref: &str,
    ) -> CredentialRealizationCapabilities {
        let mut installed = capabilities(
            selected_holder,
            CredentialRealizationKind::ProcessProtocolField,
        );
        installed.extension_consumers.insert(
            format!(
                "{}{}",
                awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
                backend_ref
            ),
            BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
        );
        installed
    }

    fn session_runtime_with_mcp(
        credential: Option<CredentialAccess>,
        selected_plaintext_holder: Option<PlaintextHolder>,
    ) -> crate::SessionRuntimeEnvelope {
        let worker = holder(PlaintextBoundary::Worker, "worker-a");
        let mcp_holder = selected_plaintext_holder
            .clone()
            .unwrap_or_else(|| worker.clone());
        crate::SessionRuntimeEnvelope::from_projection(
            awaken_session_contract::EnvironmentSnapshot {
                environment_id: "environment-a".into(),
                revision: awaken_session_contract::EnvironmentRevision(1),
                self_hosted: true,
                config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                    "environment-a@1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                    inference_holder: worker.clone(),
                    mcp_holder,
                    resource_holder: worker,
                },
            },
            Some(Default::default()),
            vec![awaken_session_contract::StageMcpAttachment {
                workspace_id: "workspace-a".into(),
                generation: awaken_session_contract::McpGenerationRef {
                    session_id: "session-a".into(),
                    attachment_id: awaken_session_contract::McpAttachmentId("mcp-a".into()),
                    generation: awaken_session_contract::McpGeneration(1),
                    runtime_incarnation: "worker-a".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: 10_000,
                },
                realization_id: "realization-a".into(),
                stage_idempotency_key: "stage-a".into(),
                name: "mcp-a".into(),
                target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test")
                    .expect("valid MCP target"),
                prompts_as_skills: false,
                credential,
                selected_plaintext_holder,
            }],
        )
        .expect("Session runtime projection serializes")
    }

    #[test]
    fn session_mcp_credentials_join_the_existing_claim_admission() {
        // Cause/effect graph: C1 credential/holder are absent or paired; C2
        // usage is canonical Authorization Bearer; C3 holder is legacy Worker,
        // ACP Workload, or unsupported Platform; C4 every effective backend is
        // ACP for Workload delivery; C5 one installed profile contains the exact
        // holder/source/ProcessProtocolField plus that backend's declaration.
        // Effects: E1 credential-free, legacy relay, and exact client injection
        // admit without adding an MCP attempt binding; E2 malformed/unsupported
        // requests fail before Worker effects; E3 capability failures retain the
        // common admission error. Constraint: alternative profiles never combine
        // a backend declaration from one adapter with mechanism evidence from
        // another; the Session generation remains holder/receipt authority.
        //
        // | Rule | binding | usage | holder/backend | installed evidence | Effect |
        // | M1 | none | - | - | - | E1 credential-free |
        // | M2 | paired | bearer | Worker | exact relay | E1 legacy relay |
        // | M3 | paired | bearer | Workload/all ACP | same-profile exact marker | E1 client injection |
        // | M4 | unpaired | - | - | - | E2 invalid binding |
        // | M5 | paired | other | Worker | exact relay | E2 invalid usage |
        // | M6 | paired | bearer | Platform | otherwise exact | E2 unsupported holder |
        // | M7 | paired | bearer | Workload/non-ACP | otherwise exact | E2 unsupported backend |
        // | M8 | paired | bearer | Workload/ACP | missing mechanism | E3 holder/mechanism unsupported |
        // | M9 | paired | bearer | Workload/ACP | missing source | E3 source unsupported |
        // | M10 | paired | bearer | Workload/ACP | marker names another backend | E2 unsupported adapter |
        // | M11 | paired | bearer | Workload/ACP | marker/mechanism split across profiles | E2 no cross-profile synthesis |
        let worker = holder(PlaintextBoundary::Worker, "worker-a");
        let workload = holder(PlaintextBoundary::Workload, "workload-a");
        let platform = holder(PlaintextBoundary::Platform, "platform-a");
        let access = |selected: &PlaintextHolder, usage| {
            CredentialAccess::new(
                CredentialRef {
                    id: "mcp-credential".into(),
                    revision: 3,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                CredentialExecutionPolicy::exact(selected.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::McpAuthorization,
                "https://mcp.example.test/",
            ))
        };
        let worker_installed = capabilities(&worker, CredentialRealizationKind::WorkerRelay);

        let mut anonymous = a_request();
        anonymous.session_runtime = Some(session_runtime_with_mcp(None, None));
        assert_eq!(
            compile_attempt_credential_bindings(&anonymous, &worker_installed, 1, 10),
            Ok(Vec::new()),
            "M1"
        );

        let worker_bearer = access(
            &worker,
            CredentialUsage::HttpHeader {
                name: "Authorization".into(),
                scheme: Some("Bearer".into()),
            },
        );
        let mut legacy_relay = a_request();
        legacy_relay.session_runtime = Some(session_runtime_with_mcp(
            Some(worker_bearer.clone()),
            Some(worker.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&legacy_relay, &worker_installed, 1, 10),
            Ok(Vec::new()),
            "M2"
        );

        let workload_bearer = access(
            &workload,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("bearer".into()),
            },
        );
        let workload_installed = acp_mcp_capabilities(&workload, "acp:test");
        let mut client_injection = a_request();
        client_injection.session_runtime = Some(session_runtime_with_mcp(
            Some(workload_bearer.clone()),
            Some(workload.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &workload_installed, 1, 10),
            Ok(Vec::new()),
            "M3"
        );

        let mut unpaired = a_request();
        unpaired.session_runtime =
            Some(session_runtime_with_mcp(Some(worker_bearer.clone()), None));
        assert_eq!(
            compile_attempt_credential_bindings(&unpaired, &worker_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialBinding),
            "M4"
        );

        let mut invalid_usage = a_request();
        invalid_usage.session_runtime = Some(session_runtime_with_mcp(
            Some(access(&worker, CredentialUsage::ProviderAdapter)),
            Some(worker.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&invalid_usage, &worker_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage),
            "M5"
        );

        let mut platform_request = a_request();
        platform_request.session_runtime = Some(session_runtime_with_mcp(
            Some(access(
                &platform,
                CredentialUsage::HttpHeader {
                    name: "Authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            )),
            Some(platform.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(
                &platform_request,
                &capabilities(&platform, CredentialRealizationKind::PlatformRelay),
                1,
                10,
            ),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M6"
        );

        let mut non_acp = client_injection.clone();
        non_acp.activation.snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "prov", "model", "genai",
            ));
        assert_eq!(
            compile_attempt_credential_bindings(&non_acp, &workload_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M7"
        );

        let mut missing_kind = workload_installed.clone();
        missing_kind.realization_kinds.clear();
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &missing_kind, 1, 10),
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::HolderUnsupported,
            )),
            "M8"
        );

        let mut missing_source = workload_installed.clone();
        missing_source.material_sources.clear();
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &missing_source, 1, 10),
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::MaterialSourceUnsupported,
            )),
            "M9"
        );

        let mismatched_adapter = acp_mcp_capabilities(&workload, "acp:other");
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &mismatched_adapter, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M10"
        );

        let mechanism_only =
            capabilities(&workload, CredentialRealizationKind::ProcessProtocolField);
        let mut marker_only = CredentialRealizationCapabilities::default();
        marker_only.extension_consumers.insert(
            format!(
                "{}{}",
                awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
                "acp:test"
            ),
            BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
        );
        let split_profiles =
            CredentialRealizationCapabilities::alternatives([mechanism_only, marker_only]);
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &split_profiles, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M11"
        );
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
    /// | B10 | T | T | T | T | Remote/Worker | T | T | worker relay |
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
                expected: BindingExpected::One(CredentialRealizationKind::WorkerRelay),
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
                    CredentialRealizationKind::WorkerRelay,
                ),
                _ => (
                    "genai",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerProviderAdapter,
                ),
            };
            let usage = if matches!(
                rule.fixture,
                BindingFixture::InvalidUsage | BindingFixture::RemoteWorker
            ) {
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                }
            } else {
                CredentialUsage::ProviderAdapter
            };
            let policy = if matches!(rule.fixture, BindingFixture::HolderForbidden) {
                CredentialExecutionPolicy::exact(
                    holder(PlaintextBoundary::Worker, "worker-b"),
                    ModelExposurePolicy::Forbidden,
                )
            } else {
                CredentialExecutionPolicy::exact(
                    selected_holder.clone(),
                    ModelExposurePolicy::Forbidden,
                )
            };
            let mut access = CredentialAccess::new(
                CredentialRef {
                    id: "credential-a".into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                policy,
            )
            .with_target(if matches!(rule.fixture, BindingFixture::RemoteWorker) {
                awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
                    "https://agent.invalid",
                )
            } else {
                awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    "provider-a",
                )
            });
            if matches!(rule.fixture, BindingFixture::EnvelopeExpired) {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "envelope-a".into(),
                        payload_fingerprint: "sha256:payload".into(),
                    },
                    recipient: TrustDomainRef(selected_holder.trust_domain.0.clone()),
                    expires_at_unix_ms: 9,
                });
            }
            if matches!(rule.fixture, BindingFixture::RefreshRevisionMismatch) {
                access = access.with_refresh(CredentialRefreshAccess::new(
                    8,
                    "https://auth.invalid/token".into(),
                    "client-a".into(),
                    TokenEndpointAuth::None,
                    None,
                    "refresh-a".into(),
                    "access-a".into(),
                    None,
                    None,
                ));
            }
            let candidate = candidate_with_access("provider-a", "model-a", backend, access);
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
                    assert_eq!(
                        actual,
                        Err(DispatchCredentialAdmissionError::Attempt(expected)),
                        "{}",
                        rule.id
                    );
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
        // Test design. Causes: C1 each persisted status uses the exact canonical
        // spelling; C2 the value is case-variant, unknown, or empty. Effects: E1
        // C1 maps to its public state; E2 C2 is rejected. Constraint/Invariant:
        // unrecognized durable vocabulary must never become runnable. Decision rule:
        // cover every supported state plus one representative from each
        // invalid class (case mismatch, unknown, and empty).
        assert_eq!(
            DispatchState::from_db("reserved"),
            Some(DispatchState::Reserved)
        );
        assert_eq!(
            DispatchState::from_db("reservation_running"),
            Some(DispatchState::ReservationLeased)
        );
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
            context_messages: Vec::new(),
            result: ResumeResult::allow(),
        }
    }

    #[test]
    fn pending_input_round_trips_through_serde() {
        // Cause/effect graph: C1 a current row carries stable System context;
        // C2 a legacy row omits the field. Effects: E1 current values round-trip
        // without changing role/id/content; E2 omission decodes to empty. The
        // PendingInput is the one durable delivery payload, so no second System
        // queue or Session context store participates.
        //
        // | Rule | context field | Effect |
        // |---|---|---|
        // | P1 | present, one System Message | E1 exact round-trip |
        // | P2 | absent | E2 empty compatibility default |
        // Constraint/Invariant: PendingInput remains the sole durable delivery
        // payload, and legacy decoding may not invent context. Decision rule:
        // execute P1 and P2 once to cover both field-presence partitions.
        let mut p = pending();
        p.context_messages = vec![Message::text(
            MessageId("system-1".into()),
            Role::System,
            "context",
        )];
        let json = serde_json::to_string(&p).expect("serializes");
        let back: PendingInput = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, p, "P1/E1");

        let mut legacy = serde_json::to_value(&p).expect("to value");
        legacy
            .as_object_mut()
            .expect("object")
            .remove("context_messages");
        let legacy: PendingInput = serde_json::from_value(legacy).expect("legacy row loads");
        assert!(legacy.context_messages.is_empty(), "P2/E2");
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
        // Test design. Causes: C1 a claimed payload carries a bound sandbox and
        // cancellation/recovery flags; C2 the same payload is unplaced. Effects:
        // E1 C1 round-trips every authority-bearing field; E2 C2 preserves
        // sandbox=None. Constraint/Invariant: transport serialization must not
        // alter claim ownership or synthesize placement. Decision rule: exercise
        // the bound and unbound sandbox partitions over the same claim.
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
            session_activity_admission_required: false,
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
            _claim: &RunClaim,
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
        async fn quarantine_retry_exhausted(&self, _: u64, _: u64) -> Result<usize, DispatchError> {
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
