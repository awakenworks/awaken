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

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};

pub(crate) fn worker_admin_router_with_lifecycle(lifecycle: Arc<crate::WorkerLifecycle>) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(lifecycle_readyz))
        .route("/metrics", get(metrics))
        .route("/admin/drain", post(lifecycle_drain))
        .with_state(lifecycle)
}

async fn lifecycle_readyz(
    State(lifecycle): State<Arc<crate::WorkerLifecycle>>,
) -> impl IntoResponse {
    awaken_server::admin::readyz(lifecycle.host.pool_accepting_work())
}

async fn lifecycle_drain(
    State(lifecycle): State<Arc<crate::WorkerLifecycle>>,
) -> impl IntoResponse {
    match lifecycle.begin_drain(None).await {
        Ok(()) => (StatusCode::OK, "draining\n"),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "draining locally; registry unavailable\n",
        ),
    }
}

async fn livez() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// The worker's Prometheus scrape: the whole process's OTel metrics (the
/// `awaken.dispatch.*` throughput its runs record, plus model/tool metrics). The
/// worker's load signal is that throughput; whether it is draining is the `/readyz`
/// 503, not a metric.
async fn metrics() -> impl IntoResponse {
    (StatusCode::OK, awaken_observability::render_prometheus())
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    // A host with no dispatch pool (a bare direct host) is never "accepting work", so
    // readiness reports 503 — the worker is not routable until its pool is up.
    #[tokio::test]
    async fn readyz_is_503_without_a_running_pool() {
        let host = Arc::new(awaken_server::SharedHost::new(
            Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
            "worker-test",
        ));
        let lifecycle = Arc::new(crate::WorkerLifecycle {
            host,
            control: awaken_runtime_host::WorkerControlClient::new(
                awaken_runtime_host::WorkerUpstream::new("http://127.0.0.1:1")
                    .with_worker_id("admin-test"),
            ),
            identity: awaken_worker_contract::WorkerIdentity::new("admin-test", "boot", 1),
        });
        let app = worker_admin_router_with_lifecycle(lifecycle);
        assert_eq!(
            call(&app, "GET", "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Liveness is independent of readiness — the process is up.
        assert_eq!(call(&app, "GET", "/livez").await.0, StatusCode::OK);
        // Registry acknowledgement is unavailable, so HTTP fails closed while the
        // lifecycle still closes the local claim gate.
        assert_eq!(
            call(&app, "POST", "/admin/drain").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // /metrics renders the process's Prometheus scrape (the global OTel registry).
        // In this unit test no meter provider is installed, so it is an empty 200 —
        // the render path is exercised at the observability level. The worker's real
        // load signal is its `awaken.dispatch.*` throughput, exposed once the process
        // has installed the meter provider via `awaken_observability::init`.
        let (status, _body) = call(&app, "GET", "/metrics").await;
        assert_eq!(status, StatusCode::OK);
    }
}
