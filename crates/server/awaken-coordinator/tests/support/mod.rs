use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// Wait for one read-only Managed event projection without becoming another
/// execution driver. When a POST receipt is available, only events at or after
/// its last accepted Event may satisfy the predicate, so a terminal Event from
/// an earlier Run cannot produce a false positive.
pub async fn wait_for_session_events(
    app: &Router,
    session_id: &str,
    accepted_receipt: Option<&Value>,
    expectation: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Value {
    // Causes: C1 the lifecycle supervisor has not projected the accepted command
    // yet; C2 it has committed and projected the receipt anchor plus the expected
    // effects; C3 the bounded observation deadline expires.
    // Effects: E1 retry only the read projection after yielding; E2 return the
    // complete public Event list; E3 fail with the latest committed projection.
    // Constraints/invariants: this helper never executes or reconciles a Run;
    // the SessionApplication lifecycle supervisor remains the sole driver, and
    // receipt anchoring excludes stale terminal Events from earlier commands.
    // Decision rules: W1=C1 -> E1; W2=C2 -> E2; W3=C3 -> E3.
    let receipt_anchor = accepted_receipt.map(|receipt| {
        receipt["data"]
            .as_array()
            .and_then(|events| events.last())
            .and_then(|event| event["id"].as_str())
            .unwrap_or_else(|| panic!("accepted Event receipt has no anchor: {receipt}"))
            .to_string()
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let uri = format!("/v1/sessions/{session_id}/events?limit=500");

    loop {
        let request = Request::builder()
            .method("GET")
            .uri(&uri)
            .body(Body::empty())
            .expect("build Managed Event list request");
        let response = app
            .clone()
            .oneshot(request)
            .await
            .expect("read Managed Event projection");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect Managed Event projection")
            .to_bytes();
        let latest = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|error| {
            panic!(
                "GET {uri} returned non-JSON ({error}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert_eq!(status, StatusCode::OK, "GET {uri}: {latest}");

        let events = latest["data"]
            .as_array()
            .unwrap_or_else(|| panic!("GET {uri} has no Event data: {latest}"));
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
