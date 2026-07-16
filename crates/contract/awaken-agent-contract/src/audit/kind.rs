use serde::{Deserialize, Serialize};

/// The committed **observability / lifecycle** event stream (persisted to
/// `{prefix}_event`). These are a run's durable lifecycle facts — phase changes,
/// state-change markers, park/resume, permission and continuation decisions —
/// and exist for audit and read-side projection, **not** as a source of truth.
/// Committed message content lives in `{prefix}_message`, committed state in
/// `{prefix}_state_command`, and the run-fact phase authority / append fence in
/// `{prefix}_commit`. The event stream never reconstructs message or state truth,
/// and resume/read-your-writes never read it — so there is deliberately no
/// `MessageCommitted`/`StateCommitted` fact here (that would imply the stream is
/// the message log, which it is not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    RunPhaseChanged,
    StateChanged,
    // A run parked on a structured waiting reason, and its later resume. The
    // design dropped the "external tool" concept: a client-executed tool is just
    // one waiting reason, so these names stay neutral (wait/resume).
    RunWaiting,
    RunResumed,
    /// A protected tool call passed the permission gate; the payload records the
    /// decision (allow/deny/ask) for audit (ADR-0030).
    PermissionDecided,
    /// A run-end continuation guard decided one round at a natural-end boundary.
    /// The payload is the guard's opaque `detail` (the kernel does not interpret
    /// it); committed with the run so the round history is durable truth, not a
    /// best-effort stream event.
    Continuation,
}
