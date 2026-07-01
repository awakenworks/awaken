use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    RunPhaseChanged,
    MessageCommitted,
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
