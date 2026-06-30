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
    /// next wake.
    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError>;
}

/// Durable pending-input intake for a thread.
#[async_trait]
pub trait PendingInbox: Send + Sync {
    /// Idempotently append one pending input. Returns `true` if newly stored,
    /// `false` if the `message_id` was already present (duplicate delivery).
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError>;
}

/// The combined durable-ingress store. One object implements both aggregates so
/// a wake can freeze pending input within a claim; the worker and host depend on
/// this bundle, not on a concrete store.
pub trait DispatchStore: RunDispatch + PendingInbox {}

impl<T: RunDispatch + PendingInbox> DispatchStore for T {}
