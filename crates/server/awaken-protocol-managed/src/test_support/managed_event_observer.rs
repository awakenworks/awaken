use std::future::Future;
use std::time::Duration;

use serde_json::Value;

use crate::state::COMMITTED_PREFIX_READ_CONFLICT;

/// Session lifecycle Events name a boundary some Run reached, never which
/// command caused it. `session.status_idle` published by a concurrent
/// background Run is indistinguishable from the one this test's command
/// produced -- that is exactly how the Zhike sales suite became ~80% flaky.
/// A lifecycle Event is therefore only ever a *refinement*; it is refused as
/// the fact a wait settles on unless the command's own processed marker is
/// required alongside it.
const LIFECYCLE_PREFIX: &str = "session.status";

/// The committed value one Managed Event wait settles on.
///
/// A wait's settle condition is a *fact* -- a value some writer committed --
/// never "an Event of some type arrived" and never elapsed time. Reading the
/// committed marker is what makes a wait reproducible: it is attributable to
/// the command under test, so no concurrent Run can satisfy it early and no
/// change of *which* component does the work can satisfy it late.
///
/// Three facts exist, in decreasing strength:
///
/// * [`DurableFact::command_processed`] -- this exact command carries a
///   committed `processed_at`. The root writes that marker together with the
///   Run's Session-activity removal, so it names *this* command.
/// * [`DurableFact::committed_event_after`] -- an Event of one exact
///   content-bearing type is committed in this receipt's causal slice. For a
///   command whose own Run deliberately parks (a required action), the parked
///   command's `processed_at` is not the fact under test.
/// * [`DurableFact::committed_event`] -- an Event of one exact content-bearing
///   type is in the committed projection, with no command to anchor on (a
///   Deployment seed, a Dream's auxiliary Session).
///
/// A caller may narrow any of them with a further assertion over the committed
/// slice. A refinement can only make the wait wait longer; it can never settle
/// the wait on its own, so no refinement can reintroduce the generic-signal
/// defect this type exists to remove.
#[derive(Clone, Copy)]
pub struct DurableFact<'a> {
    anchor: Option<&'a Value>,
    require_processed: bool,
    committed_type: Option<&'static str>,
}

impl<'a> DurableFact<'a> {
    /// Settle on the accepted command's own committed `processed_at`.
    #[must_use]
    pub fn command_processed(accepted_receipt: &'a Value) -> Self {
        Self {
            anchor: Some(accepted_receipt),
            require_processed: true,
            committed_type: None,
        }
    }

    /// Settle on one exact content-bearing Event type committed at or after
    /// `accepted_receipt`'s own Event.
    ///
    /// # Panics
    /// If `event_type` is a Session lifecycle Event: those carry no command
    /// identity, so use [`DurableFact::command_processed`] and name the
    /// lifecycle shape as a refinement instead.
    #[must_use]
    pub fn committed_event_after(accepted_receipt: &'a Value, event_type: &'static str) -> Self {
        Self {
            anchor: Some(accepted_receipt),
            require_processed: false,
            committed_type: Some(content_bearing(event_type)),
        }
    }

    /// Settle on one exact content-bearing Event type committed anywhere in the
    /// Session's projection, for a Session this test issued no command to.
    ///
    /// # Panics
    /// If `event_type` is a Session lifecycle Event.
    #[must_use]
    pub fn committed_event(event_type: &'static str) -> Self {
        Self {
            anchor: None,
            require_processed: false,
            committed_type: Some(content_bearing(event_type)),
        }
    }

    fn anchor_id(&self) -> Option<String> {
        self.anchor.map(|receipt| {
            receipt["data"]
                .as_array()
                .and_then(|events| events.last())
                .and_then(|event| event["id"].as_str())
                .unwrap_or_else(|| panic!("accepted Event receipt has no anchor: {receipt}"))
                .to_string()
        })
    }

    /// Is this fact committed in `events`, the receipt-anchored causal slice?
    fn holds(&self, events: &[Value]) -> bool {
        if self.require_processed
            && !events
                .first()
                .is_some_and(|accepted| accepted["processed_at"].is_string())
        {
            return false;
        }
        self.committed_type
            .is_none_or(|committed_type| events.iter().any(|event| event["type"] == committed_type))
    }

    fn describe(&self) -> String {
        match (self.require_processed, self.committed_type) {
            (true, _) => "the command's committed processed_at".into(),
            (_, Some(committed_type)) => format!("a committed {committed_type} Event"),
            (false, None) => unreachable!("every DurableFact constructor names a committed value"),
        }
    }
}

fn content_bearing(event_type: &'static str) -> &'static str {
    assert!(
        !event_type.starts_with(LIFECYCLE_PREFIX),
        "{event_type} is a Session lifecycle Event, not a command-attributable fact; \
         settle on DurableFact::command_processed(&receipt) and name the lifecycle \
         shape as a refinement"
    );
    event_type
}

/// Observe one Managed Event projection until `fact` is committed, without
/// becoming a second lifecycle driver. The caller supplies the HTTP GET
/// adapter; this protocol-owned loop decides which wire observation is safe to
/// retry, and `refinement` narrows -- never replaces -- the settle condition.
pub async fn wait_for_session_events<F, Fut, P>(
    fact: DurableFact<'_>,
    timeout: Duration,
    expectation: &str,
    mut read: F,
    refinement: P,
) -> Value
where
    F: FnMut() -> Fut,
    Fut: Future<Output = (u16, Value)>,
    P: Fn(&[Value]) -> bool,
{
    let receipt_anchor = fact.anchor_id();
    let deadline = tokio::time::Instant::now() + timeout;
    let settled = fact.describe();

    loop {
        let (status, latest) = read().await;
        if is_transient_committed_prefix_conflict(status, &latest) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {expectation} to reach {settled} after receipt {receipt_anchor:?}; latest projection read: {latest}"
            );
            tokio::task::yield_now().await;
            continue;
        }
        assert_eq!(
            status, 200,
            "Managed Event projection read failed: {latest}"
        );

        let events = latest["data"]
            .as_array()
            .unwrap_or_else(|| panic!("Managed Event projection has no Event data: {latest}"));
        let causal_events = receipt_anchor.as_ref().and_then(|anchor| {
            events
                .iter()
                .position(|event| event["id"] == anchor.as_str())
                .map(|position| &events[position..])
        });
        let causal_events = match (&receipt_anchor, causal_events) {
            (Some(_), Some(events)) => Some(events),
            (Some(_), None) => None,
            (None, _) => Some(events.as_slice()),
        };
        if causal_events.is_some_and(|events| fact.holds(events) && refinement(events)) {
            return latest;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expectation} to reach {settled} after receipt {receipt_anchor:?}; latest projection: {latest}"
        );
        tokio::task::yield_now().await;
    }
}

/// Settle on the durable fact that every committed lifecycle fact has been
/// delivered: the Session repository's outbox holds no pending row.
///
/// The outbox row is completed only after its delivery succeeded, so an empty
/// outbox is the exact committed evidence that the receiver already has every
/// fact the product owed it. Counting arrivals receiver-side instead settles on
/// "enough have arrived", which is satisfied while a further delivery is still
/// in flight -- an exactness assertion made against that signal is a race, not
/// a check.
///
/// # Panics
/// If the repository read fails, or the outbox is still draining at `timeout`.
pub async fn wait_for_delivered_lifecycle_facts(
    repository: &dyn awaken_session_contract::ManagedSessionRepository,
    timeout: Duration,
    expectation: &str,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let pending = repository
            .pending_lifecycle()
            .await
            .expect("read the Session repository's lifecycle outbox");
        if pending.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expectation}: {} lifecycle fact(s) undelivered",
            pending.len()
        );
        tokio::task::yield_now().await;
    }
}

fn is_transient_committed_prefix_conflict(status: u16, body: &Value) -> bool {
    status == 503
        && body["error"]["type"] == "api_error"
        && body["error"]["message"] == COMMITTED_PREFIX_READ_CONFLICT
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    #[test]
    fn transient_conflict_classifier_requires_the_exact_wire_error() {
        // Cause/effect decision table: C1 status is 503; C2 type is api_error;
        // C3 message is the committed-prefix conflict. E1 retry only when all
        // causes hold; E2 reject every partial or unrelated match. Rules:
        // R1=C1+C2+C3=>E1; R2=!C1||!C2||!C3=>E2.
        let exact = json!({"error": {
            "type": "api_error",
            "message": COMMITTED_PREFIX_READ_CONFLICT,
        }});
        assert!(is_transient_committed_prefix_conflict(503, &exact), "R1/E1");
        assert!(
            !is_transient_committed_prefix_conflict(500, &exact),
            "R2/E2"
        );
        assert!(
            !is_transient_committed_prefix_conflict(
                503,
                &json!({"error": {
                    "type": "invalid_request_error",
                    "message": COMMITTED_PREFIX_READ_CONFLICT,
                }}),
            ),
            "R2/E2"
        );
        assert!(
            !is_transient_committed_prefix_conflict(
                503,
                &json!({"error": {"type": "api_error", "message": "dependency unavailable"}}),
            ),
            "R2/E2"
        );
    }

    #[tokio::test]
    async fn observer_retries_the_transient_get_and_preserves_receipt_causality() {
        // Cause/effect decision table: C1 the first GET sees the exact transient
        // conflict; C2 the next projection contains a stale terminal before the
        // receipt anchor; C3 the anchored command carries its committed
        // processed_at and the expected terminal follows it. E1 retry only the
        // GET; E2 ignore the stale terminal; E3 return the full page after C3.
        // Rule R1=C1+C2+C3=>E1+E2+E3.
        let responses = Arc::new(Mutex::new(VecDeque::from([
            (
                503,
                json!({"error": {
                    "type": "api_error",
                    "message": COMMITTED_PREFIX_READ_CONFLICT,
                }}),
            ),
            (
                200,
                json!({"data": [
                    {"id": "old", "type": "session.status_idle"},
                    {"id": "anchor", "type": "user.message", "processed_at": "2026-09-18T00:00:00Z"},
                    {"id": "done", "type": "session.status_idle"},
                ]}),
            ),
        ])));
        let receipt = json!({"data": [{"id": "anchor"}]});
        let observed = wait_for_session_events(
            DurableFact::command_processed(&receipt),
            Duration::from_millis(50),
            "receipt-anchored idle boundary",
            || {
                let response = responses
                    .lock()
                    .expect("response queue")
                    .pop_front()
                    .expect("bounded observation response");
                async move { response }
            },
            |events| {
                events
                    .iter()
                    .any(|event| event["type"] == "session.status_idle")
            },
        )
        .await;
        assert_eq!(observed["data"][2]["id"], "done", "R1/E3");
        assert!(
            responses.lock().expect("response queue").is_empty(),
            "R1/E1"
        );
    }

    #[tokio::test]
    async fn an_unprocessed_command_never_settles_on_a_foreign_idle() {
        // This is the exact regression shape: a concurrent background Run
        // publishes session.status_idle after this command's receipt while the
        // command itself is still queued. Causes: C1 the anchored command has
        // no committed processed_at; C2 a later idle Event satisfies the
        // caller's refinement. Effects: E1 the wait does not settle; E2 the
        // command's own processed marker, once committed, settles it.
        // Rules: R1=C1+C2=>E1; R2=!C1+C2=>E2.
        let pending = json!({"data": [
            {"id": "anchor", "type": "user.message", "processed_at": null},
            {"id": "foreign", "type": "session.status_idle"},
        ]});
        let receipt = json!({"data": [{"id": "anchor"}]});
        let fact = DurableFact::command_processed(&receipt);
        let task = tokio::spawn(async move {
            let receipt = json!({"data": [{"id": "anchor"}]});
            wait_for_session_events(
                DurableFact::command_processed(&receipt),
                Duration::from_millis(20),
                "this command's own turn",
                || {
                    let pending = json!({"data": [
                        {"id": "anchor", "type": "user.message", "processed_at": null},
                        {"id": "foreign", "type": "session.status_idle"},
                    ]});
                    async move { (200, pending) }
                },
                |events| {
                    events
                        .iter()
                        .any(|event| event["type"] == "session.status_idle")
                },
            )
            .await
        });
        assert!(task.await.expect_err("R1/E1").is_panic(), "R1/E1");
        assert!(
            !fact.holds(pending["data"].as_array().expect("slice")),
            "R1/E1"
        );
        let processed = json!([
            {"id": "anchor", "type": "user.message", "processed_at": "2026-09-18T00:00:00Z"},
            {"id": "own", "type": "session.status_idle"},
        ]);
        assert!(fact.holds(processed.as_array().expect("slice")), "R2/E2");
    }

    #[test]
    fn a_lifecycle_event_is_refused_as_the_fact_a_wait_settles_on() {
        // Causes: C1 the named type is a Session lifecycle Event; C2 it is a
        // content-bearing Event. Effects: E1 refuse construction; E2 accept.
        // Rules: R1=C1=>E1; R2=C2=>E2.
        assert!(
            std::panic::catch_unwind(|| DurableFact::committed_event("session.status_idle"))
                .is_err(),
            "R1/E1"
        );
        let receipt = json!({"data": [{"id": "anchor"}]});
        assert!(
            std::panic::catch_unwind(|| DurableFact::committed_event_after(
                &receipt,
                "session.status_failed"
            ))
            .is_err(),
            "R1/E1"
        );
        let fact = DurableFact::committed_event("agent.message");
        assert!(fact.holds(&[json!({"type": "agent.message"})]), "R2/E2");
        assert!(
            !fact.holds(&[json!({"type": "session.status_idle"})]),
            "R2/E2"
        );
    }

    #[tokio::test]
    async fn observer_does_not_retry_an_unrelated_http_failure() {
        // Cause/effect decision table: C1 a read returns a non-success status;
        // C2 its envelope is not the exact committed-prefix conflict. E1 fail
        // immediately; E2 do not issue a second GET. Rule R1=C1+C2=>E1+E2.
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_reads = reads.clone();
        let task = tokio::spawn(async move {
            wait_for_session_events(
                DurableFact::committed_event("agent.message"),
                Duration::from_millis(50),
                "unrelated failure",
                move || {
                    task_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async {
                        (
                            503,
                            json!({"error": {
                                "type": "api_error",
                                "message": "dependency unavailable",
                            }}),
                        )
                    }
                },
                |_| false,
            )
            .await
        });
        assert!(task.await.expect_err("R1/E1").is_panic(), "R1/E1");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1, "R1/E2");
    }

    #[tokio::test]
    async fn observer_rejects_a_malformed_success_page() {
        // Cause/effect decision table: C1 status is 200; C2 data is absent or is
        // not an Event array. E1 fail immediately rather than treating malformed
        // wire state as an empty pending projection. Rule R1=C1+C2=>E1.
        let task = tokio::spawn(async {
            wait_for_session_events(
                DurableFact::committed_event("agent.message"),
                Duration::from_millis(50),
                "malformed projection",
                || async { (200, json!({"data": "not-an-array"})) },
                |_| false,
            )
            .await
        });
        assert!(task.await.expect_err("R1/E1").is_panic(), "R1/E1");
    }
}
