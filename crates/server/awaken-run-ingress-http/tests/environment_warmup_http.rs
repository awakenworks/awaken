use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_run_ingress_http::worker_environment_warmup_router_with_clock;
use awaken_run_ingress_testkit::worker_http::ready_worker;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, ManualWorkerClock};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

struct WarmupSource {
    result: Result<Vec<awaken_session_contract::EnvironmentSnapshot>, String>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_contract::EnvironmentWarmupSource for WarmupSource {
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_contract::EnvironmentSnapshot>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

async fn post(router: &Router, worker: &str, identity: Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/worker/environment/warmups")
                .header("content-type", "application/json")
                .header("x-awaken-worker-id", worker)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "identity": identity })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

/// Warmup projection FMECA and cause/effect decision table:
/// F1 a catalog/readiness outage is projected as an empty desired set, causing
/// every Worker to discard valid capacity (S6 O4 D3, RPN72); F2 an invalid or
/// stale identity reaches the source and learns Environment configuration
/// (S8 O3 D3, RPN72). C1 exact current identity; C2 source succeeds; C3 source
/// fails. Effects: E1 return the authoritative list, E2 return 500 rather than
/// false emptiness, E3 reject before source access.
/// | Rule | C1 | C2 | C3 | Effect |
/// | W1   | 1  | 1  | 0  | E1     |
/// | W2   | 1  | 0  | 1  | E2     |
/// | W3   | 0  | -  | -  | E3     |
#[tokio::test]
async fn warmup_projection_preserves_source_failure_and_identity_fencing() {
    let (directory, identity) = ready_worker("warmup-worker").await;
    let success = Arc::new(WarmupSource {
        result: Ok(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let success_router = worker_environment_warmup_router_with_clock(
        success.clone(),
        directory.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualWorkerClock::new(0)),
    );

    let (status, body) = post(
        &success_router,
        identity.worker_id.as_str(),
        serde_json::to_value(&identity).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "W1: {body}");
    assert_eq!(body["warmups"], json!([]), "W1");

    let mut wrong_identity = identity.clone();
    wrong_identity.incarnation_id = "stale-incarnation".into();
    let (status, _) = post(
        &success_router,
        identity.worker_id.as_str(),
        serde_json::to_value(wrong_identity).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "W3");
    assert_eq!(success.calls.load(Ordering::SeqCst), 1, "W3");

    let failure = Arc::new(WarmupSource {
        result: Err("catalog unavailable".into()),
        calls: AtomicUsize::new(0),
    });
    let failure_router = worker_environment_warmup_router_with_clock(
        failure.clone(),
        directory,
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualWorkerClock::new(0)),
    );
    let (status, body) = post(
        &failure_router,
        identity.worker_id.as_str(),
        serde_json::to_value(&identity).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "W2: {body}");
    assert_eq!(failure.calls.load(Ordering::SeqCst), 1, "W2");
}
