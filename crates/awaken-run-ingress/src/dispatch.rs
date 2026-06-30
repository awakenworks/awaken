//! The durable-ingress store ports and their data.
//!
//! Two aggregates, per the run-ingress DDD split:
//!
//! - [`RunDispatch`] owns *delivery opportunity* — enqueue, single-owner claim,
//!   lease, settle, and lease-expiry recovery. It never owns message payload
//!   truth or a run's outcome; those are the thread and run aggregates, read back
//!   from committed facts.
//! - [`PendingInbox`] owns the *thread's pending input* — idempotent append of
//!   delivered-but-unconsumed input, frozen once at a safe run boundary.
//!
//! One concrete store implements both (so a wake can freeze pending input inside
//! the same claim transaction), but the traits stay split so neither aggregate
//! reaches into the other's invariants. A blanket [`DispatchStore`] bundles them
//! for the worker and host.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use serde::{Deserialize, Serialize};

use crate::request::RunExecutionRequest;

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
    /// The waiting-ticket correlation this input answers. Consumption is keyed to
    /// it: the worker delivers an input only while the committed ticket still
    /// carries the same correlation, so a resume that already committed (and
    /// advanced or cleared the ticket) is never re-applied (ADR-0010).
    pub correlation_id: String,
    /// Earliest delivery time (epoch millis). `None` is deliverable immediately;
    /// a future time schedules the wake — the claim skips it until it is due and
    /// the daemon's poll fires it when the clock reaches it (ADR-0014).
    #[serde(default)]
    pub available_at_ms: Option<u64>,
    /// What this input delivers back into the parked run on resume.
    pub result: ResumeResult,
}

/// A lease over one claimed dispatch: the single owner allowed to execute this
/// run until `expires_ms`. An expired lease is reclaimable (recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run_id: RunId,
    pub owner: String,
    pub expires_ms: u64,
}

/// A claimed, ready-to-run dispatch and the run's undelivered pending input.
/// `pending` is empty for a fresh run and non-empty for a wake.
#[derive(Debug, Clone, PartialEq)]
pub struct Claimed {
    pub request: RunExecutionRequest,
    pub lease: Lease,
    /// The run's undelivered pending input. The worker decides execute-vs-resume
    /// from committed truth (the waiting ticket), not from this field, and tells
    /// `settle` which inputs it consumed.
    pub pending: Vec<PendingInput>,
}

/// How a claimed attempt resolved. Settled atomically with releasing the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The run reached a terminus; the dispatch is finished and removed.
    Done,
    /// The run parked on a waiting ticket; keep the dispatch for a later wake.
    Parked,
}

/// Durable run-dispatch queue: activation opportunity, claim, lease, recovery.
#[async_trait]
pub trait RunDispatch: Send + Sync {
    /// Idempotently record an accepted run. Re-enqueueing the same run id is a
    /// no-op, so an at-least-once submit has an exactly-once effect per run.
    async fn enqueue(&self, request: RunExecutionRequest) -> Result<(), DispatchError>;

    /// Claim one runnable dispatch for `owner`, single owner per run: a fresh
    /// `pending` run, a parked run with pending input (a wake), or a running
    /// dispatch whose lease expired (recovery). Returns `None` when nothing is
    /// runnable, and the run's current pending input in the returned [`Claimed`].
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Settle a claimed dispatch. `Done` removes it and all its pending input;
    /// `Parked` returns it to the waiting state and drops only the `consumed`
    /// pending (by `message_id`), leaving input that arrived mid-attempt for the
    /// next wake. `Parked` also resets the crash-retry budget — a run that
    /// reaches a checkpoint refreshes its attempts.
    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError>;

    /// Dead-letter every *crashed* dispatch — one whose lease expired without a
    /// settle — that has used up its crash-retry budget (`attempt_count >=
    /// max_attempts`). A dead-lettered dispatch is no longer claimed, so a poison
    /// run cannot be reclaimed forever. Returns how many were dead-lettered
    /// (ADR-0015). The crash-retry count increments only on recovery re-claims, so
    /// a normal park/wake never spends the budget.
    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError>;

    /// The run ids currently dead-lettered, for operations.
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Return a dead-lettered run to the queue at a fresh budget. Returns `true`
    /// if a dead-lettered run with that id was requeued.
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError>;

    /// Durably cancel a *not-running* dispatch (pending or parked): remove it and
    /// its pending input so it never runs or resumes. Returns the run's thread id
    /// when cancelled (the host then commits a terminal `Cancelled` fact), or
    /// `None` if the run is currently running (use live cancel), already
    /// dead-lettered, or unknown.
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError>;

    /// The run currently parked on a thread, if any. A thread is the stable
    /// addressable unit (a run is one ephemeral execution); this resolves a
    /// thread-addressed delivery to the run waiting on it.
    async fn parked_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError>;
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
pub trait PendingInbox: Send + Sync {
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
pub trait MessageOutbox: Send + Sync {
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
pub trait DispatchStore: RunDispatch + PendingInbox + MessageOutbox {}

impl<T: RunDispatch + PendingInbox + MessageOutbox> DispatchStore for T {}
