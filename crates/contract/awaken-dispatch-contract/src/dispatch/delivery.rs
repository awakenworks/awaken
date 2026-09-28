// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Inbox/Outbox ports and PendingInput moved together from dispatch.rs with their wire
// @formal reason: tests. Atomic relay-and-enqueue remains one backend transaction; this module split adds
// @formal reason: no store or formal-proof claim.
// Pending-input and cross-Thread delivery contracts, included in `dispatch` like
// its existing queue contract. Preserve public type paths and the one backend
// transaction that joins delivery to dispatch.
use awaken_runtime_contract::resume::ResumeResult;

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
/// operations surface; these are not direct-attempt or product routes.
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

/// Atomic Session continuation operations over the canonical Outbox and
/// Dispatch row. Database-less Worker transports do not implement this role.
#[async_trait]
pub trait SessionContinuationOutbox: Send + Sync {
    /// Atomically stage one bound resume input and rotate the Dispatch row's
    /// Session activity coordinate. Exact replay is inert.
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: Option<u64>,
    ) -> Result<bool, DispatchError>;

    /// Atomically append one immediate cross-Thread input and enqueue the exact
    /// Run under its root/Session-child admission authority.
    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError>;
}

#[cfg(test)]
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

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

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
}
