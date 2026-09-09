use std::future::Future;
use std::time::Duration;

use serde_json::Value;

use crate::state::constants::COMMITTED_PREFIX_READ_CONFLICT;

/// Observe one optionally receipt-anchored Managed Event projection without
/// becoming a second lifecycle driver. The caller supplies the HTTP GET adapter;
/// this protocol-owned loop decides which wire observation is safe to retry.
pub async fn wait_for_session_events<F, Fut, P>(
    accepted_receipt: Option<&Value>,
    timeout: Duration,
    expectation: &str,
    mut read: F,
    predicate: P,
) -> Value
where
    F: FnMut() -> Fut,
    Fut: Future<Output = (u16, Value)>,
    P: Fn(&[Value]) -> bool,
{
    let receipt_anchor = accepted_receipt.map(|receipt| {
        receipt["data"]
            .as_array()
            .and_then(|events| events.last())
            .and_then(|event| event["id"].as_str())
            .unwrap_or_else(|| panic!("accepted Event receipt has no anchor: {receipt}"))
            .to_string()
    });
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let (status, latest) = read().await;
        if is_transient_committed_prefix_conflict(status, &latest) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {expectation} after receipt {receipt_anchor:?}; latest projection read: {latest}"
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
        if causal_events.is_some_and(&predicate) {
            return latest;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expectation} after receipt {receipt_anchor:?}; latest projection: {latest}"
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
        // receipt anchor; C3 it contains the expected terminal after the anchor.
        // E1 retry only the GET; E2 ignore the stale terminal; E3 return the full
        // page after C3. Rule R1=C1+C2+C3=>E1+E2+E3.
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
                    {"id": "anchor", "type": "user.message"},
                    {"id": "done", "type": "session.status_idle"},
                ]}),
            ),
        ])));
        let receipt = json!({"data": [{"id": "anchor"}]});
        let observed = wait_for_session_events(
            Some(&receipt),
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
    async fn observer_does_not_retry_an_unrelated_http_failure() {
        // Cause/effect decision table: C1 a read returns a non-success status;
        // C2 its envelope is not the exact committed-prefix conflict. E1 fail
        // immediately; E2 do not issue a second GET. Rule R1=C1+C2=>E1+E2.
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_reads = reads.clone();
        let task = tokio::spawn(async move {
            wait_for_session_events(
                None,
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
                None,
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
