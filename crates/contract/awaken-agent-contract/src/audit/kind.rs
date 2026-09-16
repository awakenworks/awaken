use serde::{Deserialize, Serialize};

/// The committed **observability / lifecycle** event stream (persisted to
/// `{prefix}_event`). These are a run's durable lifecycle facts — state changes,
/// state-change markers, await/resume, permission and continuation decisions —
/// and exist for audit and read-side projection, **not** as a source of truth.
/// Committed message content lives in `{prefix}_message`, committed state in
/// `{prefix}_state_command`, and the run-fact state authority / append fence in
/// `{prefix}_commit`. The event stream never reconstructs message or state truth,
/// and resume/read-your-writes never read it — so there is deliberately no
/// `MessageCommitted`/`StateCommitted` fact here (that would imply the stream is
/// the message log, which it is not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    RunStateChanged,
    /// A durable dispatch recovery admitted another execution attempt for this
    /// still-active Run. The dispatch aggregate remains retry authority; this
    /// claim-fenced audit fact makes that transition observable through the
    /// existing committed lifecycle feed without a second event store.
    RunRescheduled,
    /// One logical model request completed. Provider retries remain internal to
    /// that request and are summarized by the event payload's `retry_count`.
    /// This is an observation only: messages and accumulated usage retain their
    /// existing committed authorities.
    ModelRequestCompleted,
    StateChanged,
    // A run awaits for a structured reason and may later resume. The
    // design dropped the "external tool" concept: a client-executed tool is just
    // one awaiting reason, so these names stay neutral (wait/resume).
    RunAwaiting,
    RunResumed,
    /// A protected tool call passed the permission gate; the payload records the
    /// decision (allow/deny/ask) for audit (ADR-0030).
    PermissionDecided,
    /// A person answered a call the gate had routed to them; the payload
    /// records their `allow`/`deny`. A distinct fact from the gate's decision:
    /// the decision says what the authored policy concluded, the confirmation
    /// records how the wait it opened was answered (ADR-0030 D5).
    PermissionConfirmed,
    /// One durable ingress operation consumed its exact committed resume
    /// authority. The operation id is the existing PendingInput message id;
    /// this receipt distinguishes exact replay after the active ticket is gone.
    ResumeApplied,
    /// A run-end continuation guard decided one round at a natural-end boundary.
    /// The payload is the guard's opaque `detail` (the kernel does not interpret
    /// it); committed with the run so the round history is durable truth, not a
    /// best-effort stream event.
    Continuation,
}

#[cfg(test)]
mod tests {
    use super::Kind;

    #[test]
    fn only_current_lifecycle_kind_names_enter_the_domain() {
        // Causes: C1 a current kind name reaches the domain decoder; C2 a
        // retired persisted name bypasses the store migration boundary.
        // Effects: E1 C1 round-trips exactly; E2 C2 is rejected. Decision table:
        // R1=C1->E1; R2=C2->E2. Historical repair belongs only to each store's
        // versioned migration, never to the public domain enum.
        let state: Kind = serde_json::from_str("\"RunStateChanged\"").unwrap();
        let awaiting: Kind = serde_json::from_str("\"RunAwaiting\"").unwrap();
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            "\"RunStateChanged\""
        );
        assert_eq!(serde_json::to_string(&awaiting).unwrap(), "\"RunAwaiting\"");
        assert!(serde_json::from_str::<Kind>("\"RunPhaseChanged\"").is_err());
        assert!(serde_json::from_str::<Kind>("\"RunWaiting\"").is_err());
    }
}
