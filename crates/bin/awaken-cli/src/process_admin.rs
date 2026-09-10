//! Process administration for graceful, connection-scaled Coordinator operation (ADR-0022 D7).
//!
//! The Coordinator is light IO and scales on **connection load**, not queue depth, and it
//! is resident with long-lived streams, so scale-in must **drain** rather than kill:
//!
//! - `active_streams` — a gauge of in-flight requests (dominated by long-lived
//!   SSE/event streams), scraped by the KEDA prometheus trigger to autoscale.
//! - `POST /admin/drain` — flip the Brain to draining: `/readyz` then reports 503, so
//!   the Service/gateway stops routing new work to it. The public request gate also
//!   rejects new requests on retained HTTP/1.1 or HTTP/2 connections while requests
//!   admitted before the drain continue to completion (a `preStop` hook calls this
//!   before SIGTERM; interrupted clients reconnect and resume from durable truth).
//! - `GET /readyz` — readiness for the Service: 200 normally, 503 while draining.
//!   A Coordinator backed by PostgreSQL also executes one bounded `SELECT 1`
//!   against its process-owned pool; it never opens a probe-only connection pool.
//! - `GET /metrics` — connection/drain gauges plus Control registration
//!   readiness, pending-domain, failure, and lag gauges.
//! - `GET /admin/session-event-batch-cutover-validation` — the latest
//!   secret-free legacy Event-batch cutover proof from this process's sole
//!   Session lifecycle supervisor.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

const POSTGRES_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

struct ActiveRequestGuard(Arc<DrainController>);

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}

struct ActiveResponseBody {
    inner: Body,
    _guard: ActiveRequestGuard,
}

impl http_body::Body for ActiveResponseBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        http_body::Body::poll_frame(std::pin::Pin::new(&mut self.get_mut().inner), cx)
    }

    fn is_end_stream(&self) -> bool {
        http_body::Body::is_end_stream(&self.inner)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::Body::size_hint(&self.inner)
    }
}

/// Shared drain + in-flight-connection state for one Coordinator process.
#[derive(Default)]
pub struct DrainController {
    draining: AtomicBool,
    active: AtomicUsize,
    public_admission_fence: Mutex<()>,
    registration_supervisor: RwLock<Option<Arc<awaken_control::StaticRegistrationSupervisor>>>,
    registration_health: RwLock<Option<Arc<awaken_control::RegistrationHealth>>>,
    service_lifecycle: RwLock<Option<awaken_service_lifecycle::ServiceLifecycle>>,
    postgres_pool: RwLock<Option<sqlx::PgPool>>,
    event_batch_cutover_validation:
        RwLock<Option<Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>>>,
}

impl DrainController {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// In-flight requests right now (the autoscaling signal).
    #[must_use]
    pub fn active_streams(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Whether the Coordinator has been asked to drain for scale-in/shutdown.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
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

    pub fn set_service_lifecycle(&self, lifecycle: awaken_service_lifecycle::ServiceLifecycle) {
        *self
            .service_lifecycle
            .write()
            .expect("service lifecycle lock poisoned") = Some(lifecycle);
    }

    pub(crate) fn set_session_event_batch_cutover_validation_source(
        &self,
        source: Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>,
    ) {
        *self
            .event_batch_cutover_validation
            .write()
            .expect("Session Event-batch cutover validation source lock poisoned") = Some(source);
    }

    /// Attach the canonical Coordinator pool already opened by
    /// `CoordinatorPersistence`. Readiness clones this handle; it must never
    /// create a second pool or retain a database URL.
    pub(crate) fn set_postgres_pool(&self, pool: sqlx::PgPool) {
        *self
            .postgres_pool
            .write()
            .expect("postgres readiness pool lock poisoned") = Some(pool);
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
                .service_lifecycle
                .read()
                .expect("service lifecycle lock poisoned")
                .as_ref()
                .is_none_or(awaken_service_lifecycle::ServiceLifecycle::is_healthy)
            && self
                .registration_health
                .read()
                .expect("registration health lock poisoned")
                .as_ref()
                .is_none_or(|health| health.snapshot().ready)
    }

    fn readiness_decision(&self, database_ready: bool) -> bool {
        self.is_ready() && database_ready
    }

    async fn is_serving_ready(&self) -> bool {
        // Drain and critical-task failure mask the database state. Short-circuit
        // them before acquiring a connection so an unroutable process does not
        // add load to an already degraded database.
        if !self.is_ready() {
            return false;
        }
        let pool = self
            .postgres_pool
            .read()
            .expect("postgres readiness pool lock poisoned")
            .clone();
        let Some(pool) = pool else {
            return self.readiness_decision(true);
        };
        let database_ready = bounded_readiness_probe(
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&pool),
            POSTGRES_READINESS_TIMEOUT,
        )
        .await;
        // Re-evaluate process state after the await so a concurrent drain or
        // critical-task failure cannot race a successful database response.
        self.readiness_decision(database_ready)
    }

    fn registration_snapshot(&self) -> Option<awaken_control::RegistrationHealthSnapshot> {
        self.registration_health
            .read()
            .expect("registration health lock poisoned")
            .as_ref()
            .map(|health| health.snapshot())
    }

    fn session_event_batch_cutover_validation_snapshot(
        &self,
    ) -> Option<awaken_session_application::SessionEventBatchCutoverValidationSnapshot> {
        self.event_batch_cutover_validation
            .read()
            .expect("Session Event-batch cutover validation source lock poisoned")
            .as_ref()
            .and_then(|source| source.snapshot())
    }

    fn admit_public_request(ctrl: &Arc<Self>) -> Option<ActiveRequestGuard> {
        let _fence = ctrl
            .public_admission_fence
            .lock()
            .expect("public request admission fence poisoned");
        if ctrl.is_draining() {
            return None;
        }
        ctrl.active.fetch_add(1, Ordering::AcqRel);
        Some(ActiveRequestGuard(ctrl.clone()))
    }

    fn begin_drain(&self) {
        let _fence = self
            .public_admission_fence
            .lock()
            .expect("public request admission fence poisoned");
        self.draining.store(true, Ordering::Release);
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
    let Some(guard) = DrainController::admit_public_request(&ctrl) else {
        return awaken_coordinator::admin::readyz(false).into_response();
    };
    let response = next.run(req).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(ActiveResponseBody {
            inner: body,
            _guard: guard,
        }),
    )
}

async fn drain(State(ctrl): State<Arc<DrainController>>) -> impl IntoResponse {
    ctrl.begin_drain();
    (StatusCode::OK, "draining\n")
}

async fn readyz(State(ctrl): State<Arc<DrainController>>) -> impl IntoResponse {
    awaken_coordinator::admin::readyz(ctrl.is_serving_ready().await)
}

fn session_event_batch_cutover_validation_response(
    snapshot: Option<awaken_session_application::SessionEventBatchCutoverValidationSnapshot>,
) -> Response {
    match snapshot {
        Some(snapshot) => (StatusCode::OK, Json(snapshot)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "session_event_batch_cutover_validation_pending",
            })),
        )
            .into_response(),
    }
}

async fn session_event_batch_cutover_validation(
    State(ctrl): State<Arc<DrainController>>,
) -> Response {
    session_event_batch_cutover_validation_response(
        ctrl.session_event_batch_cutover_validation_snapshot(),
    )
}

async fn bounded_readiness_probe<F, T, E>(probe: F, deadline: std::time::Duration) -> bool
where
    F: std::future::Future<Output = Result<T, E>>,
{
    matches!(tokio::time::timeout(deadline, probe).await, Ok(Ok(_)))
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
        .route(
            "/admin/session-event-batch-cutover-validation",
            get(session_event_batch_cutover_validation),
        )
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(ctrl)
}

/// Register the Coordinator's autoscaling gauges on the global OTel meter, reading `ctrl`
/// at scrape time: `awaken_brain_active_streams` (connection load) and
/// `awaken_brain_draining` (1 while draining, so an autoscaler/dashboard sees the
/// scale-in state — complementary to the `/readyz` 503 the k8s Service routes on).
/// Call once at the process startup, AFTER `awaken_observability::init`, and keep the
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
        .with_description("1 when registration projection health permits the process to serve; pending domains are reported separately.")
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
    let registration_lag = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_control_registration_lag_seconds")
            .with_description("Seconds since the last successful static-registration recovery.")
            .with_callback(move |obs| {
                if let Some(snapshot) = ctrl.registration_snapshot() {
                    obs.observe(snapshot.lag_seconds, &[]);
                }
            })
            .build()
    };
    let service_lifecycle_healthy = {
        let ctrl = ctrl.clone();
        meter
            .u64_observable_gauge("awaken_service_lifecycle_healthy")
            .with_description("1 while every supervised critical service task is healthy.")
            .with_callback(move |obs| {
                if let Some(tasks) = ctrl
                    .service_lifecycle
                    .read()
                    .expect("service lifecycle lock poisoned")
                    .as_ref()
                {
                    obs.observe(u64::from(tasks.is_healthy()), &[]);
                }
            })
            .build()
    };
    vec![
        active,
        draining,
        registration_ready,
        registration_pending,
        registration_failures,
        registration_lag,
        service_lifecycle_healthy,
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
    async fn active_request_lifetime_includes_the_response_body() {
        // Cause graph: business request -> increment -> response headers -> body
        // retained/streamed -> body drop -> decrement. Returning headers must not
        // decrement a long-lived SSE/AI SDK response prematurely.
        //
        // | Rule | Handler/body state | Expected active count |
        // | C1 | no request | 0 |
        // | C2 | response returned, body retained | 1 |
        // | C3 | body consumed or dropped | 0 |
        //
        // FMECA (S/O/D, RPN): header-lifetime accounting makes autoscaling miss
        // all active streams (9/7/8=504); decrement leakage pins a replica active
        // forever (7/3/5=105). The body-owned RAII guard covers cancellation,
        // normal completion, and dropped clients with one mechanism.
        let ctrl = DrainController::new();
        let app = with_connection_metric(
            Router::new().route("/business", axum::routing::get(|| async { "ok" })),
            ctrl.clone(),
        );
        assert_eq!(ctrl.active_streams(), 0, "C1");

        let response = app
            .oneshot(HttpRequest::get("/business").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ctrl.active_streams(), 1, "C2");

        drop(response);
        assert_eq!(ctrl.active_streams(), 0, "C3");
    }

    #[tokio::test]
    async fn retained_http1_connection_is_rejected_after_drain() {
        // Causes: C1 one real HTTP/1.1 connection completes a public request
        // with keep-alive; C2 drain then linearizes; C3 the client sends another
        // business request on that exact socket. Effects: E1 C1 is 200; E2 C3
        // crosses the per-request gate and is 503 rather than bypassing drain.
        // Rule K1=C1+!C2=>E1; K2=C1+C2+C3=>E2.
        fn read_response_status(stream: &mut std::net::TcpStream) -> u16 {
            use std::io::Read;

            let mut response = Vec::new();
            let mut byte = [0_u8; 1];
            while !response.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                response.push(byte[0]);
            }
            let headers = String::from_utf8(response).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::parse::<usize>)
                })
                .transpose()
                .unwrap()
                .unwrap_or_default();
            let mut body = vec![0_u8; content_length];
            stream.read_exact(&mut body).unwrap();
            headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap()
        }

        let ctrl = DrainController::new();
        let app = with_connection_metric(
            Router::new().route("/business", axum::routing::get(|| async { "completed" })),
            ctrl.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let (first_status, first_observed) = tokio::sync::oneshot::channel();
        let (continue_request, continue_after_drain) = std::sync::mpsc::channel();
        let (second_status, second_observed) = tokio::sync::oneshot::channel();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;

            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            for (sequence, observed) in [(1, first_status), (2, second_status)] {
                if sequence == 2 {
                    continue_after_drain.recv().unwrap();
                }
                write!(
                    stream,
                    "GET /business HTTP/1.1\r\nHost: {address}\r\nConnection: keep-alive\r\n\r\n"
                )
                .unwrap();
                observed.send(read_response_status(&mut stream)).unwrap();
            }
        });

        assert_eq!(first_observed.await.unwrap(), 200, "K1/E1");
        ctrl.begin_drain();
        continue_request.send(()).unwrap();
        assert_eq!(second_observed.await.unwrap(), 503, "K2/E2");
        client.await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn drain_completes_inflight_rejects_http2_and_preserves_private_settlement() {
        // Cause/effect graph: C1 one public request entered before drain; C2
        // drain linearizes while C1 is in flight; C3 a later HTTP/2 stream
        // reaches the same public middleware; C4 the private Worker
        // settle/registry surface uses its distinct Router. Effects: E1 C1 stays
        // counted and completes normally; E2 C3 is rejected with 503 before its
        // handler and is never counted; E3 C4 remains usable.
        //
        // | Rule | Surface | Admission time/version | Effect |
        // |---|---|---|---|
        // | D1 | public | before drain, in flight | E1 complete and count |
        // | D2 | public | after drain, HTTP/2 stream | E2 503 |
        // | D3 | private | after drain, settle/register | E3 204 |
        let ctrl = DrainController::new();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let public = with_connection_metric(
            Router::new().route(
                "/business",
                axum::routing::get({
                    let entered = entered.clone();
                    let release = release.clone();
                    move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            "completed"
                        }
                    }
                }),
            ),
            ctrl.clone(),
        );
        let private = Router::new()
            .route(
                "/private/workers/settle",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/private/workers/register",
                post(|| async { StatusCode::NO_CONTENT }),
            );

        let in_flight = tokio::spawn(
            public
                .clone()
                .oneshot(HttpRequest::get("/business").body(Body::empty()).unwrap()),
        );
        entered.notified().await;
        assert_eq!(ctrl.active_streams(), 1, "D1/E1");
        ctrl.begin_drain();

        let http2 = public
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/business")
                    .version(axum::http::Version::HTTP_2)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(http2.status(), StatusCode::SERVICE_UNAVAILABLE, "D2/E2");
        assert_eq!(ctrl.active_streams(), 1, "rejected requests are not active");

        for path in ["/private/workers/settle", "/private/workers/register"] {
            let response = private
                .clone()
                .oneshot(HttpRequest::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "D3/E3 {path}");
        }

        release.notify_one();
        let admitted = in_flight.await.unwrap().unwrap();
        assert_eq!(admitted.status(), StatusCode::OK, "D1/E1");
        assert_eq!(ctrl.active_streams(), 1, "D1 response body retains guard");
        drop(admitted);
        assert_eq!(ctrl.active_streams(), 0, "D1/E1 completed");
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
    async fn session_event_batch_cutover_validation_is_fail_closed_and_secret_free() {
        // Cause/effect graph: C1 this process has not completed an authoritative
        // final Session scan; C2 it has one published snapshot. Effects: E1 C1
        // returns 503 with a stable pending classification; E2 C2 returns 200
        // with exactly generation and the four aggregate counts. Session IDs,
        // quarantine reasons, clocks, and repository details are never inputs.
        //
        // | Rule | Snapshot | Status | Body |
        // |---|---|---|---|
        // | A1 | absent | 503 | pending classification only |
        // | A2 | present | 200 | exact five-field snapshot |
        let (app, _) = app();
        let (status, body) = get(&app, "/admin/session-event-batch-cutover-validation").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "A1/E1");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "session_event_batch_cutover_validation_pending"}),
            "A1/E1"
        );

        let response = session_event_batch_cutover_validation_response(Some(
            awaken_session_application::SessionEventBatchCutoverValidationSnapshot {
                generation: 9,
                terminal_with_incomplete_event_batches: 1,
                event_batch_failures: 2,
                quarantined: 3,
                restoring_sessions: 4,
            },
        ));
        assert_eq!(response.status(), StatusCode::OK, "A2/E2");
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "generation": 9,
                "terminal_with_incomplete_event_batches": 1,
                "event_batch_failures": 2,
                "quarantined": 3,
                "restoring_sessions": 4,
            }),
            "A2/E2"
        );
    }

    #[tokio::test]
    async fn critical_task_failure_removes_readiness() {
        // Causes: C1 service lifecycle healthy, C2 one critical task returns an
        // error, C3 drain is false. Effects: E1 C1+C3 -> ready; E2 C2+C3 ->
        // unavailable. Registration state is absent, so this isolates the one
        // service-lifecycle health source from the existing registration source.
        let (app, ctrl) = app();
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        ctrl.set_service_lifecycle(lifecycle.clone());
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK, "E1");
        lifecycle.spawn("failed", |_| async { Err("offline".into()) });
        lifecycle.wait_for_failure().await.expect("C2 observed");
        assert_eq!(
            get(&app, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "E2"
        );
        lifecycle
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .expect("failed task watcher joins");
    }

    #[tokio::test]
    async fn readiness_decision_table_covers_postgres_failure_and_timeout() {
        // Cause/effect graph:
        // C1 process accepts traffic (not draining and critical tasks healthy);
        // C2 the canonical Coordinator PgPool completes `SELECT 1`; C3 the
        // query returns an error; C4 the one-second probe deadline expires;
        // C5 a critical task fails; C6 drain begins. E1 is HTTP-ready; E2 is
        // fail-closed/unready. C3 and C4 are mutually exclusive probe outcomes;
        // C5/C6 mask database state and avoid a redundant checkout.
        //
        // | Rule | C1 | DB outcome | critical task | draining | Effect |
        // |---|---|---|---|---|---|
        // | R1 | yes | success | healthy | no | E1 ready |
        // | R2 | yes | error | healthy | no | E2 unready |
        // | R3 | yes | timeout | healthy | no | E2 unready |
        // | R4 | no | success | failed | no | E2 unready |
        // | R5 | no | success | healthy | yes | E2 unready |
        let healthy_database = bounded_readiness_probe(
            std::future::ready(Ok::<_, ()>(())),
            std::time::Duration::from_secs(1),
        )
        .await;
        let database_down = bounded_readiness_probe(
            std::future::ready(Err::<(), _>("database unavailable")),
            std::time::Duration::from_secs(1),
        )
        .await;
        let database_timeout = bounded_readiness_probe(
            std::future::pending::<Result<(), ()>>(),
            std::time::Duration::ZERO,
        )
        .await;

        let ctrl = DrainController::new();
        assert!(ctrl.readiness_decision(healthy_database), "R1");
        assert!(!ctrl.readiness_decision(database_down), "R2");
        assert!(!ctrl.readiness_decision(database_timeout), "R3");

        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        ctrl.set_service_lifecycle(lifecycle.clone());
        lifecycle.spawn("failed", |_| async { Err("offline".into()) });
        lifecycle.wait_for_failure().await.expect("C5 observed");
        assert!(!ctrl.readiness_decision(healthy_database), "R4");
        lifecycle
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .expect("failed task watcher joins");

        let draining = DrainController::new();
        draining.begin_drain();
        assert!(!draining.readiness_decision(healthy_database), "R5");
    }

    #[tokio::test]
    async fn readyz_isolates_rebuildable_registration_degradation() {
        // Cause/effect decision table: R1 a role without Control registration
        // authority has no health source -> ready; R2 Control attaches its
        // initial degraded health -> still ready while pending/failure gauges
        // carry the degradation. R3 drain/critical lifecycle failure -> 503 is
        // covered by adjacent tests; supervisor transitions remain Control-owned.
        // FMECA: making a rebuildable projection a serving-readiness dependency
        // can evict every last-known-good endpoint during a Control outage;
        // separating readiness from degraded registration metrics eliminates
        // that causal edge without adding another recovery path.
        let (app, ctrl) = app();
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK, "R1");
        ctrl.set_registration_health_for_test(Arc::new(
            awaken_control::RegistrationHealth::default(),
        ));
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK, "R2");
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
        // registration ready=1 and pending_domains=2. The supervisor test owns
        // later success/failure transitions; this test owns metric projection.
        // FMECA: if readiness and degradation share one gauge, autoscaling may
        // remove healthy last-known-good endpoints. The paired gauges preserve
        // serving readiness while making incomplete recovery detectable.
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
                    && line.trim_end().ends_with(" 1")
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
