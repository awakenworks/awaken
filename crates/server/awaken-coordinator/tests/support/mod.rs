use awaken_protocol_managed::test_support::DurableFact;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// Wait for one read-only Managed event projection without becoming another
/// execution driver. The settle condition is the caller's `DurableFact` -- a
/// committed value, never "an Event arrived" -- and `refinement` only narrows
/// it, so a terminal Event from a concurrent Run cannot produce a false
/// positive.
pub async fn wait_for_session_events(
    app: &Router,
    session_id: &str,
    fact: DurableFact<'_>,
    expectation: &str,
    refinement: impl Fn(&[Value]) -> bool,
) -> Value {
    // Causes: C1 the lifecycle supervisor has not projected the accepted command
    // yet; C2 one GET returns the exact committed-prefix conflict; C3 it has
    // committed and projected the receipt anchor plus the expected effects; C4
    // the bounded observation deadline expires. Effects: E1 retry only the GET;
    // E2 return the complete public Event list; E3 fail with the latest response.
    // Constraints/invariants: this helper never executes or reconciles a Run;
    // the SessionApplication lifecycle supervisor remains the sole driver, and
    // receipt anchoring excludes stale terminal Events from earlier commands.
    // Decision rules: W1=C1||C2 -> E1; W2=C3 -> E2; W3=C4 -> E3.
    let uri = format!("/v1/sessions/{session_id}/events?limit=500");
    let app = app.clone();
    awaken_protocol_managed::test_support::wait_for_session_events(
        fact,
        std::time::Duration::from_secs(10),
        expectation,
        move || {
            let app = app.clone();
            let uri = uri.clone();
            async move {
                let request = Request::builder()
                    .method("GET")
                    .uri(&uri)
                    .body(Body::empty())
                    .expect("build Managed Event list request");
                let response = app
                    .oneshot(request)
                    .await
                    .expect("read Managed Event projection");
                let status = response.status().as_u16();
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
                (status, latest)
            }
        },
        refinement,
    )
    .await
}
