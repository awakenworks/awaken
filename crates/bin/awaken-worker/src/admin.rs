//! The worker's cloud-native admin surface, on a port separate from any data
//! traffic (ADR-0022 D7 line; the worker mirror of the Brain's admin surface).
//!
//! A worker scales on QUEUE depth and must drain rather than be killed: an
//! in-flight run holds a lease and its side effects must complete, so scale-in
//! stops it CLAIMING new work while its current runs finish, then lets it exit.
//!
//! - `GET /livez` — liveness: the process is up (always 200 while serving).
//! - `GET /readyz` — readiness for routing: 200 while the pool is up and claiming,
//!   503 before the pool starts or once draining, so the orchestrator stops routing.
//! - `POST /admin/drain` — begin a graceful drain: stop claiming, let in-flight runs
//!   finish (a `preStop` hook calls this before SIGTERM).
//! - `GET /metrics` — the drain gauge in Prometheus text format.

use std::sync::Arc;

use awaken_server::SharedHost;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};

/// The worker admin router over the process's host (its dispatch pool carries the
/// readiness/drain state). Serve it on the admin port, never the data path.
pub fn worker_admin_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/admin/drain", post(drain))
        .with_state(host)
}

async fn livez() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

async fn readyz(State(host): State<Arc<SharedHost>>) -> impl IntoResponse {
    awaken_server::admin::readyz(host.pool_accepting_work())
}

async fn drain(State(host): State<Arc<SharedHost>>) -> impl IntoResponse {
    host.begin_pool_drain().await;
    (StatusCode::OK, "draining\n")
}

async fn metrics(State(host): State<Arc<SharedHost>>) -> impl IntoResponse {
    let draining = u64::from(!host.pool_accepting_work());
    let body = awaken_server::admin::prometheus_gauge(
        "awaken_worker_draining",
        "1 when the worker is draining for scale-in.",
        draining,
    );
    (StatusCode::OK, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(app: &Router, method: &str, path: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    // A host with no dispatch pool (a bare direct host) is never "accepting work", so
    // readiness reports 503 — the worker is not routable until its pool is up.
    #[tokio::test]
    async fn readyz_is_503_without_a_running_pool() {
        let host = Arc::new(SharedHost::new(
            Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
            "worker-test",
        ));
        let app = worker_admin_router(host);
        assert_eq!(call(&app, "GET", "/readyz").await.0, StatusCode::SERVICE_UNAVAILABLE);
        // Liveness is independent of readiness — the process is up.
        assert_eq!(call(&app, "GET", "/livez").await.0, StatusCode::OK);
        // The drain endpoint is idempotent and safe even with no pool.
        assert_eq!(call(&app, "POST", "/admin/drain").await.0, StatusCode::OK);
        // Metrics report draining=1 (not accepting work).
        let (status, body) = call(&app, "GET", "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("awaken_worker_draining 1"), "{body}");
    }
}
