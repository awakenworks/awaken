//! What an ingress implementation durably supports (G5).
//!
//! `RunIngress` has exactly two delivery semantics: direct and durable. A caller
//! reads these capabilities to know whether durable, recoverable, or replayable
//! behavior is real before relying on it; a direct ingress reports all-false and
//! fails its durable-only operations closed.

/// The durable guarantees of a selected ingress. This is a capability *report*,
/// never a routing axis: delivery-mode policy stays private to durable ingress
/// (run-ingress design, "Simplified Current Shape").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunIngressCapabilities {
    /// Submissions survive process restart.
    pub durable: bool,
    /// Unfinished work is reclaimed and retried after a crash.
    pub recoverable: bool,
    /// Committed facts can rebuild a run without the original live wiring.
    pub replayable: bool,
    /// A parked run can be woken on a durable schedule.
    pub scheduled_wake: bool,
}

impl RunIngressCapabilities {
    /// Direct ingress: in-process only, nothing durable.
    pub const DIRECT: Self = Self {
        durable: false,
        recoverable: false,
        replayable: false,
        scheduled_wake: false,
    };

    /// Durable ingress: persists, recovers, and replays. Scheduled wake is a
    /// later slice (the queue stores no timer yet), so it stays false.
    pub const DURABLE: Self = Self {
        durable: true,
        recoverable: true,
        replayable: true,
        scheduled_wake: false,
    };
}
