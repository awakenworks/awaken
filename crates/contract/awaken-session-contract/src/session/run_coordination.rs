//! Runtime-owned child Run and budget-resume coordination projections.

use awaken_agent_contract::agent::delegation::DelegationStatus;
use awaken_agent_contract::agent::run::Id as RunId;

/// Stable child-Run relationship projected at a session boundary. Runtime owns
/// the relationship; Managed and other adapters only render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedRun {
    pub run_id: RunId,
    pub parent_call_id: String,
    pub agent_id: String,
    pub status: DelegationStatus,
}

/// Stable view of the runtime-owned delegation authority after the parent Run
/// has crossed a terminal fence and reached quiescence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegatedRunSnapshot {
    pub delegated_runs: Vec<DelegatedRun>,
    /// Ordinary logical child Threads admitted under this Session's dispatch
    /// affinity. These remain distinct from synchronous delegated Runs.
    pub coordinated_thread_ids: Vec<awaken_agent_contract::agent::thread::Id>,
    /// Monotonic committed-state position from which `delegated_runs` was rebuilt.
    pub watermark: u64,
    /// Backend-wide commit high-water read only after terminal quiescence. This
    /// is the immutable first-listability anchor for the Session terminal wire
    /// projection; it is intentionally distinct from `commands.len()`.
    pub runtime_commit_cursor: u64,
}

/// Session-approved delivery that resumes one exact committed budget pause.
/// The activity epoch is newly admitted for this continuation; durable dispatch
/// implementations atomically rotate their existing row to this coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBudgetResumeDelivery {
    pub session_id: String,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub run_id: RunId,
    pub correlation_id: String,
    /// The claimed Run's next committed-operation ordinal while this exact
    /// pause is current. It is read from committed recovery truth, remains
    /// stable while Awaiting, and advances when the same Run pauses again.
    pub pause_generation: u64,
    /// Existing durable Session activity coordinate carried by a dispatch row.
    /// The application transfers it to `session_activity_epoch` atomically;
    /// foreground Runs without a row carry `None`.
    pub prior_session_activity_epoch: Option<u64>,
    pub session_activity_epoch: u64,
}

/// One committed budget pause paired with its authoritative generation.
///
/// The generation is derived from the Run recovery snapshot rather than a
/// separate counter or pause registry. This makes repeated BudgetReached
/// pauses in the same Run distinct while keeping exact update replay stable.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBudgetResumeTicket {
    pub ticket: awaken_agent_contract::agent::awaiting::ResumeTicket,
    pub pause_generation: u64,
    pub prior_session_activity_epoch: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBudgetResumeDisposition {
    /// The durable dispatch row accepted the resume and new epoch.
    Dispatched,
    /// The exact ticket was already consumed by an earlier delivery/replay.
    Stale,
}
