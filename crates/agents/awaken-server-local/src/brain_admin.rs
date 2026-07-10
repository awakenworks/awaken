//! The Brain's admin surface for graceful, connection-scaled operation (ADR-0022 D7).
//!
//! The Brain is light IO and scales on **connection load**, not queue depth, and it
//! is resident with long-lived streams, so scale-in must **drain** rather than kill:
//!
//! - `active_streams` — a gauge of in-flight requests (dominated by long-lived
//!   SSE/event streams), scraped by the KEDA prometheus trigger to autoscale.
//! - `POST /admin/drain` — flip the Brain to draining: `/readyz` then reports 503, so
//!   the Service/gateway stops routing new work to it while existing streams finish
//!   (a `preStop` hook calls this before SIGTERM; interrupted clients reconnect and
//!   resume from durable truth).
//! - `GET /readyz` — readiness for the Service: 200 normally, 503 while draining.
//! - `GET /metrics` — the two gauges in Prometheus text format.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// Shared drain + in-flight-connection state for one Brain process.
#[derive(Default)]
pub struct DrainController {
    draining: AtomicBool,
    active: AtomicUsize,
}

impl DrainController {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// In-flight requests right now (the autoscaling signal).
    #[must_use]
    pub fn active_streams(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Whether the Brain has been asked to drain for scale-in/shutdown.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    fn begin_drain(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }
}

/// Count every request while it is in flight. A normal request increments then
/// decrements almost immediately; a long-lived stream holds the count for its whole
/// lifetime, so `active_streams` tracks concurrent connection load. RAII-style: the
/// decrement runs even if the inner handler panics, via the guard's `Drop`.
async fn count_active(State(ctrl): State<Arc<DrainController>>, req: Request, next: Next) -> Response {
    struct Guard(Arc<DrainController>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    ctrl.active.fetch_add(1, Ordering::Relaxed);
    let _guard = Guard(ctrl);
    next.run(req).await
}

async fn drain(State(ctrl): State<Arc<DrainController>>) -> impl IntoResponse {
    ctrl.begin_drain();
    (StatusCode::OK, "draining\n")
}

async fn readyz(State(ctrl): State<Arc<DrainController>>) -> impl IntoResponse {
    if ctrl.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining\n")
    } else {
        (StatusCode::OK, "ready\n")
    }
}

async fn metrics(State(ctrl): State<Arc<DrainController>>) -> impl IntoResponse {
    let body = format!(
        "# HELP awaken_brain_active_streams In-flight requests (dominated by long-lived streams).\n\
         # TYPE awaken_brain_active_streams gauge\n\
         awaken_brain_active_streams {}\n\
         # HELP awaken_brain_draining 1 when the Brain is draining for scale-in.\n\
         # TYPE awaken_brain_draining gauge\n\
         awaken_brain_draining {}\n",
        ctrl.active_streams(),
        u8::from(ctrl.is_draining()),
    );
    (StatusCode::OK, body)
}

/// Layer the Brain admin surface onto a router: the connection-count metric for
/// autoscaling, and the drain endpoint + readiness for graceful scale-in.
#[must_use]
pub fn with_brain_admin(base: Router, ctrl: Arc<DrainController>) -> Router {
    // Count only real traffic — the admin probes (/metrics, /readyz, /admin/drain)
    // are frequent short polls and must not inflate the connection gauge (nor let
    // a /metrics scrape count itself).
    let counted = base.layer(middleware::from_fn_with_state(ctrl.clone(), count_active));
    let admin = Router::new()
        .route("/admin/drain", post(drain))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(ctrl);
    counted.merge(admin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn app() -> (Router, Arc<DrainController>) {
        let ctrl = DrainController::new();
        (with_brain_admin(Router::new(), ctrl.clone()), ctrl)
    }

    async fn get(app: &Router, path: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readyz_flips_to_503_after_drain() {
        let (app, _) = app();
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK);

        // Drain, then readiness reports unavailable so the Service stops routing.
        let drained = app
            .clone()
            .oneshot(HttpRequest::post("/admin/drain").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(drained.status(), StatusCode::OK);
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn metrics_expose_active_streams_and_drain_state() {
        let (app, ctrl) = app();
        let (status, body) = get(&app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        // The counter is back to zero once /metrics itself returns (its own in-flight
        // increment/decrement has balanced), and drain starts at 0.
        assert!(body.contains("awaken_brain_active_streams 0"), "{body}");
        assert!(body.contains("awaken_brain_draining 0"), "{body}");
        assert_eq!(ctrl.active_streams(), 0);

        ctrl.begin_drain();
        let (_, body) = get(&app, "/metrics").await;
        assert!(body.contains("awaken_brain_draining 1"), "{body}");
    }
}
