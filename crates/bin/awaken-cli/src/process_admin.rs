//! Process administration for graceful, connection-scaled Coordinator operation (ADR-0022 D7).
//!
//! The Coordinator is light IO and scales on **connection load**, not queue depth, and it
//! is resident with long-lived streams, so scale-in must **drain** rather than kill:
//!
//! - `active_streams` — a gauge of in-flight requests (dominated by long-lived
//!   SSE/event streams), scraped by the KEDA prometheus trigger to autoscale.
//! - `POST /admin/drain` — flip the Brain to draining: `/readyz` then reports 503, so
//!   the Service/gateway stops routing new work to it while existing streams finish
//!   (a `preStop` hook calls this before SIGTERM; interrupted clients reconnect and
//!   resume from durable truth).
//! - `GET /readyz` — readiness for the Service: 200 normally, 503 while draining.
//! - `GET /metrics` — connection/drain gauges plus Control registration
//!   readiness, pending-domain, failure, and lag gauges.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// Shared drain + in-flight-connection state for one Coordinator process.
#[derive(Default)]
pub struct DrainController {
    draining: AtomicBool,
    active: AtomicUsize,
    registration_supervisor: RwLock<Option<Arc<awaken_control::StaticRegistrationSupervisor>>>,
    registration_health: RwLock<Option<Arc<awaken_control::RegistrationHealth>>>,
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

    /// Whether the Coordinator has been asked to drain for scale-in/shutdown.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// Retain the Control-owned supervisor for the complete process lifetime and
    /// project its one health source into readiness and metrics.
    pub fn set_registration_supervisor(
        &self,
        supervisor: Arc<awaken_control::StaticRegistrationSupervisor>,
    ) {
        let health = supervisor.health();
        *self
            .registration_supervisor
            .write()
            .expect("registration supervisor lock poisoned") = Some(supervisor);
        *self
            .registration_health
            .write()
            .expect("registration health lock poisoned") = Some(health);
    }

    #[cfg(test)]
    fn set_registration_health_for_test(&self, health: Arc<awaken_control::RegistrationHealth>) {
        *self
            .registration_health
            .write()
            .expect("registration health lock poisoned") = Some(health);
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.is_draining()
            && self
                .registration_health
                .read()
                .expect("registration health lock poisoned")
                .as_ref()
                .is_none_or(|health| health.snapshot().ready)
    }

    fn registration_snapshot(&self) -> Option<awaken_control::RegistrationHealthSnapshot> {
        self.registration_health
            .read()
            .expect("registration health lock poisoned")
            .as_ref()
            .map(|health| health.snapshot())
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
    awaken_coordinator::admin::readyz(ctrl.is_ready())
}

/// The Coordinator's Prometheus scrape: the whole process's OTel metrics — the
/// process lifecycle and Control registration gauges registered on the global
/// meter at startup, plus business `gen_ai.*`/`awaken.*` metrics. `/readyz` is
/// the routing signal; gauges expose the cause to autoscalers and dashboards.
async fn metrics() -> impl IntoResponse {
    (StatusCode::OK, awaken_observability::render_prometheus())
}

/// Layer the connection-count metric onto the business router — the autoscaling
/// signal. Kept separate from the admin routes so an operator can serve the admin
/// surface on its own port (see [`process_admin_router`]) without the probe traffic
/// inflating the gauge, while the metric still wraps only real business traffic.
pub fn with_connection_metric(base: Router, ctrl: Arc<DrainController>) -> Router {
    base.layer(middleware::from_fn_with_state(ctrl, count_active))
}

/// Coordinator process routes (drain + readiness + metrics), with their own state.
/// Mount this on a SEPARATE admin port (cloud-native: probes/metrics/drain go to a
/// management port, not the business Ingress) or, for a single-port deployment,
/// merge it via [`with_process_admin`].
pub fn process_admin_router(ctrl: Arc<DrainController>) -> Router {
    Router::new()
        .route("/admin/drain", post(drain))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(ctrl)
}

/// Register the Coordinator's autoscaling gauges on the global OTel meter, reading `ctrl`
/// at scrape time: `awaken_brain_active_streams` (connection load) and
/// `awaken_brain_draining` (1 while draining, so an autoscaler/dashboard sees the
/// scale-in state — complementary to the `/readyz` 503 the k8s Service routes on).
/// Call once at the composition root, AFTER `awaken_observability::init`, and keep the
/// returned handles for the process lifetime so the observable callbacks stay live.
#[must_use]
pub fn register_active_streams_gauge(
    ctrl: Arc<DrainController>,
) -> Vec<opentelemetry::metrics::ObservableGauge<u64>> {
    let meter = opentelemetry::global::meter("awaken-brain");
    let active = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_brain_active_streams")
            .with_description("In-flight requests (dominated by long-lived streams).")
            .with_callback(move |obs| obs.observe(ctrl.active_streams() as u64, &[]))
            .build()
    };
    let draining_ctrl = ctrl.clone();
    let draining = meter
        .u64_observable_gauge("awaken_brain_draining")
        .with_description("1 while the Brain is draining for graceful scale-in, else 0.")
        .with_callback(move |obs| obs.observe(u64::from(draining_ctrl.is_draining()), &[]))
        .build();
    let registration_ready = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_control_registration_ready")
            .with_description("1 when Agent and Environment registrations are reconciled.")
            .with_callback(move |obs| {
                if let Some(snapshot) = ctrl.registration_snapshot() {
                    obs.observe(u64::from(snapshot.ready), &[]);
                }
            })
            .build()
    };
    let registration_pending = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_control_registration_pending_domains")
            .with_description("Static registration domains whose recovery remains pending.")
            .with_callback(move |obs| {
                if let Some(snapshot) = ctrl.registration_snapshot() {
                    obs.observe(snapshot.pending_domains as u64, &[]);
                }
            })
            .build()
    };
    let registration_failures = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_control_registration_consecutive_failures")
            .with_description("Consecutive static-registration recovery failures.")
            .with_callback(move |obs| {
                if let Some(snapshot) = ctrl.registration_snapshot() {
                    obs.observe(snapshot.consecutive_failures, &[]);
                }
            })
            .build()
    };
    let registration_lag = meter
        .u64_observable_gauge("awaken_control_registration_lag_seconds")
        .with_description("Seconds since the last successful static-registration recovery.")
        .with_callback(move |obs| {
            if let Some(snapshot) = ctrl.registration_snapshot() {
                obs.observe(snapshot.lag_seconds, &[]);
            }
        })
        .build();
    vec![
        active,
        draining,
        registration_ready,
        registration_pending,
        registration_failures,
        registration_lag,
    ]
}

/// Layer the process admin surface onto a single router (one-port deployment): the
/// connection-count metric for autoscaling, and the drain endpoint + readiness for
/// graceful scale-in. For a split admin port, use [`with_connection_metric`] on the
/// business router and serve [`process_admin_router`] on the admin listener instead.
pub fn with_process_admin(base: Router, ctrl: Arc<DrainController>) -> Router {
    // Count only real traffic — the admin probes (/metrics, /readyz, /admin/drain)
    // are frequent short polls and must not inflate the connection gauge (nor let
    // a /metrics scrape count itself).
    with_connection_metric(base, ctrl.clone()).merge(process_admin_router(ctrl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn app() -> (Router, Arc<DrainController>) {
        let ctrl = DrainController::new();
        (with_process_admin(Router::new(), ctrl.clone()), ctrl)
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
        // Cause/effect decision table: R1 no registration source and not
        // draining -> ready; R2 drain requested -> unavailable. Registration
        // recovery is covered independently below.
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
    async fn readyz_waits_for_control_registration_recovery() {
        // Cause/effect decision table: R1 a role without Control registration
        // authority has no health source -> ready; R2 Control attaches its
        // initial pending health -> 503. Supervisor success transitions are
        // tested at the Control component, so this test owns only probe mapping.
        let (app, ctrl) = app();
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK, "R1");
        ctrl.set_registration_health_for_test(Arc::new(
            awaken_control::RegistrationHealth::default(),
        ));
        assert_eq!(
            get(&app, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "R2"
        );
    }

    #[tokio::test]
    async fn split_admin_router_serves_probes_without_the_business_routes() {
        // The admin router on its own port serves the probes; the business router,
        // wrapped with only the connection metric, does NOT expose the admin routes
        // (they live on the separate admin port).
        let ctrl = DrainController::new();
        let admin = process_admin_router(ctrl.clone());
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
        // Cause/effect decision table: R1 zero active requests/not draining ->
        // both lifecycle gauges are zero; R2 attached initial Control health ->
        // registration ready=0 and pending_domains=2. The supervisor test owns
        // later success/failure transitions; this test owns metric projection.
        awaken_observability::init_meters(&awaken_observability::OtelConfig::default()).ok();
        let ctrl = DrainController::new();
        ctrl.set_registration_health_for_test(Arc::new(
            awaken_control::RegistrationHealth::default(),
        ));
        let _gauge = register_active_streams_gauge(ctrl.clone());
        let scrape = awaken_observability::render_prometheus();
        assert!(
            scrape
                .lines()
                .any(|l| l.starts_with("awaken_brain_active_streams")
                    && l.trim_end().ends_with(" 0")),
            "the connection-load gauge reads 0: {scrape}"
        );
        // Draining is exposed as a gauge too (autoscale/dashboard visibility),
        // complementary to the `/readyz` 503 the k8s Service routes on.
        assert!(
            scrape
                .lines()
                .any(|l| l.starts_with("awaken_brain_draining") && l.trim_end().ends_with(" 0")),
            "the draining gauge reads 0: {scrape}"
        );
        assert!(
            scrape.lines().any(|line| {
                line.starts_with("awaken_control_registration_ready")
                    && line.trim_end().ends_with(" 0")
            }),
            "R2 registration readiness is exported: {scrape}"
        );
        assert!(
            scrape.lines().any(|line| {
                line.starts_with("awaken_control_registration_pending_domains")
                    && line.trim_end().ends_with(" 2")
            }),
            "R2 pending domains are exported: {scrape}"
        );
    }
}
