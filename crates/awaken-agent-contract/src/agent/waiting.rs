//! Same-run waiting state and the durable resume ticket.
//!
//! When a run parks (a tool needs a decision, external input, a timer, …) it
//! commits a [`WaitingTicket`]: the structured correlation a later resume must
//! match before the run continues. The ticket is pure agent-domain data so it
//! survives a commit and is validated on resume, never a live handle.

use serde::{Deserialize, Serialize};

/// Why a run is parked. A client-executed tool is just one waiting reason — the
/// design keeps these neutral rather than naming an "external tool" concept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitingReason {
    ToolPermission,
    UserInput,
    BackgroundTasks,
    ExternalEvent,
    RateLimit,
    ManualPause,
    /// The run committed a `ScheduledAction` (ADR-0003 mechanism #1): a deferred
    /// action recorded in committed state, performed by the system (not decided
    /// by a human) and recovered from the committed request for consistency
    /// (ADR-0020).
    ScheduledAction,
}

/// The committed correlation for one same-run pause. A resume is accepted only
/// when its correlation, run/thread, executable snapshot, and catalog
/// fingerprint all match, and the deadline (if any) has not passed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingTicket {
    /// Idempotency/correlation key; deduplicates retries and duplicate wakes.
    pub correlation_id: String,
    pub run_id: crate::agent::run::Id,
    pub thread_id: crate::agent::thread::Id,
    /// Executable snapshot id and catalog fingerprint, proving a resume result
    /// belongs to the same executable configuration (held as plain ids so the
    /// agent-domain contract stays independent of runtime-facing types).
    pub snapshot_id: String,
    pub catalog_fingerprint: String,
    pub reason: WaitingReason,
    /// The tool call awaiting a result, when the wait is a tool decision.
    pub call_id: Option<String>,
    /// The pending tool call, kept so an `allow` decision can execute it on
    /// resume. Held as id + JSON args (not a runtime-facing `ToolCall`).
    #[serde(default)]
    pub pending_tool: Option<PendingTool>,
    /// Optional expiry (epoch millis). A resume after this is stale.
    pub deadline_ms: Option<u64>,
}

/// The tool call a `ToolPermission` wait is holding, in pure data form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingTool {
    pub tool_id: String,
    pub arguments: serde_json::Value,
}
