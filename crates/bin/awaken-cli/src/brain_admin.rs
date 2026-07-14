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
async fn count_active(
    State(ctrl): State<Arc<DrainController>>,
    req: Request,
    next: Next,
) -> Response {
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
    awaken_server::admin::readyz(!ctrl.is_draining())
}

/// The Brain's Prometheus scrape: the whole process's OTel metrics — the
/// `awaken_brain_active_streams` connection-load gauge (registered on the global
/// meter at startup) plus the business `gen_ai.*`/`awaken.*` metrics. Whether the
/// Brain is draining is the `/readyz` 503, not a metric.
async fn metrics() -> impl IntoResponse {
    (StatusCode::OK, awaken_observability::render_prometheus())
}

/// Layer the connection-count metric onto the business router — the autoscaling
/// signal. Kept separate from the admin routes so an operator can serve the admin
/// surface on its own port (see [`brain_admin_router`]) without the probe traffic
/// inflating the gauge, while the metric still wraps only real business traffic.
pub fn with_connection_metric(base: Router, ctrl: Arc<DrainController>) -> Router {
    base.layer(middleware::from_fn_with_state(ctrl, count_active))
}

/// The Brain admin routes (drain + readiness + metrics), with their own state.
/// Mount this on a SEPARATE admin port (cloud-native: probes/metrics/drain go to a
/// management port, not the business Ingress) or, for a single-port deployment,
/// merge it via [`with_brain_admin`].
pub fn brain_admin_router(ctrl: Arc<DrainController>) -> Router {
    Router::new()
        .route("/admin/drain", post(drain))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(ctrl)
}

/// Register the `awaken_brain_active_streams` connection-load gauge on the global
/// OTel meter, reading `ctrl` at scrape time (the `/metrics` autoscaling signal).
/// Call once at the composition root, AFTER `awaken_observability::init`, and keep the
/// returned handle for the process lifetime so the observable callback stays live.
#[must_use]
pub fn register_active_streams_gauge(
    ctrl: Arc<DrainController>,
) -> opentelemetry::metrics::ObservableGauge<u64> {
    opentelemetry::global::meter("awaken-brain")
        .u64_observable_gauge("awaken_brain_active_streams")
        .with_description("In-flight requests (dominated by long-lived streams).")
        .with_callback(move |obs| obs.observe(ctrl.active_streams() as u64, &[]))
        .build()
}

/// Layer the Brain admin surface onto a single router (one-port deployment): the
/// connection-count metric for autoscaling, and the drain endpoint + readiness for
/// graceful scale-in. For a split admin port, use [`with_connection_metric`] on the
/// business router and serve [`brain_admin_router`] on the admin listener instead.
pub fn with_brain_admin(base: Router, ctrl: Arc<DrainController>) -> Router {
    // Count only real traffic — the admin probes (/metrics, /readyz, /admin/drain)
    // are frequent short polls and must not inflate the connection gauge (nor let
    // a /metrics scrape count itself).
    with_connection_metric(base, ctrl.clone()).merge(brain_admin_router(ctrl))
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readyz_flips_to_503_after_drain() {
        let (app, _) = app();
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK);

        // Drain, then readiness reports unavailable so the Service stops routing.
        let drained = app
            .clone()
            .oneshot(
                HttpRequest::post("/admin/drain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(drained.status(), StatusCode::OK);
        assert_eq!(
            get(&app, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn split_admin_router_serves_probes_without_the_business_routes() {
        // The admin router on its own port serves the probes; the business router,
        // wrapped with only the connection metric, does NOT expose the admin routes
        // (they live on the separate admin port).
        let ctrl = DrainController::new();
        let admin = brain_admin_router(ctrl.clone());
        assert_eq!(get(&admin, "/readyz").await.0, StatusCode::OK);
        // /metrics renders the global Prometheus scrape (200); its content is the
        // process's OTel metrics, exercised at the observability + e2e level.
        assert_eq!(get(&admin, "/metrics").await.0, StatusCode::OK);

        let business = with_connection_metric(Router::new(), ctrl.clone());
        // The business router has no /readyz (it's on the admin port) → 404.
        assert_eq!(get(&business, "/readyz").await.0, StatusCode::NOT_FOUND);

        // Draining via the admin router flips the shared controller the business
        // router's metric also reads.
        admin
            .clone()
            .oneshot(
                HttpRequest::post("/admin/drain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(ctrl.is_draining());
        assert_eq!(
            get(&admin, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn active_streams_gauge_is_registered_on_the_global_meter_and_scrapeable() {
        // The connection-load gauge is a composition-root concern (registered on the
        // global OTel meter), rendered via the process Prometheus scrape — not
        // embedded in the router. Install a Prometheus-only provider, register it, and
        // confirm the scrape reflects the controller's live value.
        awaken_observability::init_meters(&awaken_observability::OtelConfig::default()).ok();
        let ctrl = DrainController::new();
        let _gauge = register_active_streams_gauge(ctrl.clone());
        let scrape = awaken_observability::render_prometheus();
        assert!(
            scrape
                .lines()
                .any(|l| l.starts_with("awaken_brain_active_streams")
                    && l.trim_end().ends_with(" 0")),
            "the connection-load gauge reads 0: {scrape}"
        );
        // Draining is NOT a metric — it is the `/readyz` 503 signal (#4).
        assert!(!scrape.contains("awaken_brain_draining"), "{scrape}");
    }
}
