// @formal entrypoint managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
//! Admit by Org bucket: the access edge's request-limit decision for every
//! HTTP protocol entry.
//!
//! The edge supplies the authenticated Workspace coordinate. Open/local
//! deployments map every Workspace to their one configured Org
//! ([`OrgRateLimiter`]); hosted deployments resolve it through their IAM
//! tenant authority behind [`OrgRequestAdmission`] before taking an Org bucket.
//! A protocol contributes only an [`OrgAdmissionEncoding`]: which of its routes
//! consume which bucket, and how it encodes each outcome on its own wire.

use std::sync::Arc;
use std::time::Duration;

use awaken_runtime_contract::runs::request::{
    OrgAdmissionDecision, OrgAdmissionUnavailable, OrgOperation,
};
use awaken_sync::LeafLock;
use awaken_tenancy::org_admission::{
    OrgAdmissionRequest, OrgRateLimits, OrgRequestAdmission, OrgRequestSource,
};
use awaken_time::WallClockExt as _;
use awaken_time::mono::Stopwatch;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const MILLIS_PER_SECOND: u64 = 1_000;
const TOKEN_QUANTA_PER_TOKEN: u64 = 60 * MILLIS_PER_SECOND;
const TOKEN_QUANTA_PER_TOKEN_U32: u32 = TOKEN_QUANTA_PER_TOKEN as u32;
/// Explicit product admission bound, over 54 times the documented 1,200/minute
/// read limit. It keeps exact fixed-point arithmetic and its proof domain finite.
const MAX_ORGANIZATION_RATE_LIMIT: u32 = u16::MAX as u32;

#[derive(Debug)]
struct Bucket {
    capacity: u16,
    token_quanta: u64,
    last_refill: Duration,
}

impl Bucket {
    fn new(capacity: u32) -> Self {
        assert!(
            (1..=MAX_ORGANIZATION_RATE_LIMIT).contains(&capacity),
            "an Org rate limit must be within the admitted product domain"
        );
        let capacity = u16::try_from(capacity).expect("admission bound fits u16");
        Self {
            capacity,
            token_quanta: capacity_quanta(capacity),
            last_refill: Duration::ZERO,
        }
    }

    /// `now` is monotonic bucket time; `wall_now_ms` is the same instant on
    /// the wall clock, from which the rendered reset instant is derived.
    fn take(&mut self, now: Duration, wall_now_ms: u64) -> OrgAdmissionDecision {
        let elapsed = now.saturating_sub(self.last_refill);
        let elapsed_millis = elapsed
            .as_millis()
            .min(u128::from(TOKEN_QUANTA_PER_TOKEN_U32)) as u32;
        let refill = refill_quanta_for_elapsed(self.capacity, elapsed_millis);
        self.token_quanta = clamp_refilled_tokens(self.capacity, self.token_quanta, refill);
        self.last_refill = now;

        let (allowed, remaining_quanta) = consume_available_token(self.token_quanta);
        self.token_quanta = remaining_quanta;
        let retry_after = if allowed {
            None
        } else {
            Some(seconds_for_quanta(
                TOKEN_QUANTA_PER_TOKEN - self.token_quanta,
                self.capacity,
            ))
        };
        let reset_after = seconds_for_quanta(
            capacity_quanta(self.capacity) - self.token_quanta,
            self.capacity,
        );
        OrgAdmissionDecision {
            allowed,
            limit: u32::from(self.capacity),
            remaining: (self.token_quanta / TOKEN_QUANTA_PER_TOKEN) as u32,
            retry_after,
            reset_after,
            reset_at_unix_ms: wall_now_ms.saturating_add(reset_after.saturating_mul(1_000)),
        }
    }
}

/// Amount credited by one monotonic elapsed interval. Once a complete refill
/// period has elapsed the clamp must yield capacity, so every `>= 60s` duration
/// is represented by that canonical value. This both avoids needless floating
/// point growth and gives the verifier a bounded equivalent state space.
const fn capacity_quanta(capacity: u16) -> u64 {
    capacity as u64 * TOKEN_QUANTA_PER_TOKEN
}

fn refill_quanta_for_elapsed(capacity: u16, elapsed_millis: u32) -> u64 {
    if elapsed_millis >= TOKEN_QUANTA_PER_TOKEN_U32 {
        capacity_quanta(capacity)
    } else {
        u64::from(capacity) * u64::from(elapsed_millis)
    }
}

fn clamp_refilled_tokens(capacity: u16, tokens: u64, refill: u64) -> u64 {
    let available = capacity_quanta(capacity) - tokens;
    if refill >= available {
        capacity_quanta(capacity)
    } else {
        tokens + refill
    }
}

fn consume_available_token(tokens: u64) -> (bool, u64) {
    if tokens >= TOKEN_QUANTA_PER_TOKEN {
        (true, tokens - TOKEN_QUANTA_PER_TOKEN)
    } else {
        (false, tokens)
    }
}

fn seconds_for_quanta(quanta: u64, capacity: u16) -> u64 {
    let quanta_per_second = u64::from(capacity) * MILLIS_PER_SECOND;
    quanta.div_ceil(quanta_per_second).max(1)
}

#[cfg(kani)]
/// @obligation managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn organization_bucket_refill_is_exact_and_bounded() {
    let capacity: u16 = kani::any();
    let elapsed_millis: u32 = kani::any();
    kani::assume(capacity > 0);
    kani::assume(elapsed_millis < TOKEN_QUANTA_PER_TOKEN_U32);

    let refill = refill_quanta_for_elapsed(capacity, elapsed_millis);
    assert_eq!(refill, u64::from(capacity) * u64::from(elapsed_millis));
    // Kani's generated arithmetic property proves this exact multiplication
    // cannot overflow in the admitted domain. The independent clamp harness
    // proves the resulting bucket state never exceeds capacity.
}

#[cfg(kani)]
/// @obligation managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn organization_bucket_saturated_refill_is_canonical_and_bounded() {
    let capacity: u16 = kani::any();
    let elapsed_millis: u32 = kani::any();
    kani::assume(capacity > 0);
    kani::assume(elapsed_millis >= TOKEN_QUANTA_PER_TOKEN_U32);

    let refill = refill_quanta_for_elapsed(capacity, elapsed_millis);
    assert_eq!(refill, capacity_quanta(capacity));
}

#[cfg(kani)]
/// @obligation managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn organization_bucket_refill_clamp_preserves_capacity_invariant() {
    let capacity_u16: u16 = kani::any();
    let token_whole: u16 = kani::any();
    let token_fraction: u16 = kani::any();
    let refill_whole: u16 = kani::any();
    let refill_fraction: u16 = kani::any();
    kani::assume(capacity_u16 > 0);
    kani::assume(token_whole <= capacity_u16);
    kani::assume(refill_whole <= capacity_u16);
    kani::assume(u64::from(token_fraction) < TOKEN_QUANTA_PER_TOKEN);
    kani::assume(u64::from(refill_fraction) < TOKEN_QUANTA_PER_TOKEN);
    kani::assume(token_whole < capacity_u16 || token_fraction == 0);
    kani::assume(refill_whole < capacity_u16 || refill_fraction == 0);
    let tokens = u64::from(token_whole) * TOKEN_QUANTA_PER_TOKEN + u64::from(token_fraction);
    let refill = u64::from(refill_whole) * TOKEN_QUANTA_PER_TOKEN + u64::from(refill_fraction);

    let refilled = clamp_refilled_tokens(capacity_u16, tokens, refill);
    assert!(refilled >= tokens);
    assert!(refilled <= capacity_quanta(capacity_u16));
    if refill < capacity_quanta(capacity_u16) - tokens {
        assert_eq!(refilled, tokens + refill);
    } else {
        assert_eq!(refilled, capacity_quanta(capacity_u16));
    }
}

#[cfg(kani)]
/// @obligation managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn organization_bucket_consumption_is_exact_and_non_over_admitting() {
    let capacity_u16: u16 = kani::any();
    let whole: u16 = kani::any();
    let fraction: u16 = kani::any();
    kani::assume(capacity_u16 > 0);
    kani::assume(whole <= capacity_u16);
    kani::assume(u64::from(fraction) < TOKEN_QUANTA_PER_TOKEN);
    kani::assume(whole < capacity_u16 || fraction == 0);
    let tokens = u64::from(whole) * TOKEN_QUANTA_PER_TOKEN + u64::from(fraction);

    let (allowed, remaining) = consume_available_token(tokens);
    assert_eq!(allowed, tokens >= TOKEN_QUANTA_PER_TOKEN);
    assert!(remaining <= tokens);
    if allowed {
        assert_eq!(remaining, tokens - TOKEN_QUANTA_PER_TOKEN);
    } else {
        assert_eq!(remaining, tokens);
    }
}

#[derive(Debug)]
struct Buckets {
    create: Bucket,
    read: Bucket,
}

/// The open Org limiter: one Create and one Read bucket for the one Org a
/// process serves. Hosted deployments resolve the Org per Workspace behind
/// their own [`OrgRequestAdmission`].
pub struct OrgRateLimiter {
    organization_id: String,
    /// Monotonic bucket time: refill is judged on elapsed time only.
    started_at: Stopwatch,
    /// The composing process's wall clock; only the rendered reset instant
    /// reads it.
    clock: Arc<dyn awaken_time::WallClock>,
    buckets: LeafLock<Buckets>,
}

impl std::fmt::Debug for OrgRateLimiter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OrgRateLimiter")
            .field("organization_id", &self.organization_id)
            .field("buckets", &self.buckets)
            .finish_non_exhaustive()
    }
}

impl OrgRateLimiter {
    #[must_use]
    pub fn for_organization(
        organization_id: impl Into<String>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self::with_limits(organization_id, OrgRateLimits::default(), clock)
    }

    #[must_use]
    pub fn with_limits(
        organization_id: impl Into<String>,
        limits: OrgRateLimits,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            organization_id: organization_id.into(),
            started_at: Stopwatch::start(),
            clock,
            buckets: LeafLock::new(Buckets {
                create: Bucket::new(limits.create_per_minute),
                read: Bucket::new(limits.read_per_minute),
            }),
        }
    }

    fn check(&self, operation: OrgOperation) -> OrgAdmissionDecision {
        self.check_at(operation, self.started_at.elapsed(), self.clock.now_ms())
    }

    /// Admit a Session created internally by a Deployment. Scheduled/manual
    /// deployment launches bypass HTTP but consume the same organization Create
    /// bucket as `POST /v1/sessions`.
    fn check_at(
        &self,
        operation: OrgOperation,
        now: Duration,
        wall_now_ms: u64,
    ) -> OrgAdmissionDecision {
        let mut buckets = self.buckets.lock();
        match operation {
            OrgOperation::Create => buckets.create.take(now, wall_now_ms),
            OrgOperation::Read => buckets.read.take(now, wall_now_ms),
        }
    }

    #[must_use]
    pub fn organization_id(&self) -> &str {
        &self.organization_id
    }
}

#[async_trait::async_trait]
impl OrgRequestAdmission for OrgRateLimiter {
    async fn admit(
        &self,
        request: OrgAdmissionRequest,
    ) -> Result<OrgAdmissionDecision, OrgAdmissionUnavailable> {
        Ok(self.check(request.operation))
    }
}

/// What one protocol contributes to Org admission: which of its routes consume
/// which bucket, and how it encodes each outcome on its own wire. The edge owns
/// the decision; the protocol owns only classification and encoding.
#[derive(Clone, Copy)]
pub struct OrgAdmissionEncoding {
    /// The bucket and resource family a request consumes; `None` passes it
    /// through unmetered.
    pub classify: fn(&Method, &str) -> Option<(OrgOperation, &'static str)>,
    /// A metered request to `path` reached the edge without a trusted
    /// Workspace scope.
    pub scope_missing: fn(&str) -> Response,
    /// The Org limiter could not decide a request to `path`.
    pub unavailable: fn(&str, &OrgAdmissionUnavailable) -> Response,
    /// The bucket refused a request to `path`.
    pub rejected: fn(&str, OrgOperation, &OrgAdmissionDecision) -> Response,
    /// Bucket state the protocol publishes on every metered response, when it
    /// has a wire shape for it.
    pub annotate: Option<fn(&mut HeaderMap, &OrgAdmissionDecision)>,
}

impl OrgAdmissionEncoding {
    /// Select the common plain-text HTTP error shape with a protocol's own
    /// route classification. This adds no bucket decision or annotation policy.
    #[must_use]
    pub const fn plain_text(
        classify: fn(&Method, &str) -> Option<(OrgOperation, &'static str)>,
    ) -> Self {
        Self {
            classify,
            scope_missing: plain_text_scope_missing,
            unavailable: plain_text_unavailable,
            rejected: plain_text_rejected,
            annotate: None,
        }
    }
}

fn plain_text_scope_missing(_path: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "request has no trusted Workspace scope",
    )
        .into_response()
}

fn plain_text_unavailable(_path: &str, error: &OrgAdmissionUnavailable) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
}

fn plain_text_rejected(
    _path: &str,
    operation: OrgOperation,
    decision: &OrgAdmissionDecision,
) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        format!("Org {operation:?} request limit exceeded"),
    )
        .into_response();
    if let Some(retry_after) = decision.retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
    }
    response
}

/// The outcome of admitting one request; the protocol encodes all but
/// `Unmetered` and `Admitted`, which run the handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgAdmission {
    /// The protocol does not meter this route.
    Unmetered,
    /// Metered, but no trusted Workspace scope: refused before the limiter, so
    /// no caller-invented tenant ever reaches a bucket.
    ScopeMissing,
    /// The limiter could not decide: refused, never admitted.
    Unavailable(OrgAdmissionUnavailable),
    Admitted(OrgAdmissionDecision),
    Rejected(OrgOperation, OrgAdmissionDecision),
}

/// Decide one request against its Org bucket. Fail closed: only an explicit
/// allowed decision for a scoped request runs the handler of a metered route.
pub async fn decide_org_admission(
    admission: &dyn OrgRequestAdmission,
    class: Option<(OrgOperation, &'static str)>,
    workspace_id: Option<String>,
    operation_id: Option<String>,
) -> OrgAdmission {
    let Some((operation, resource)) = class else {
        return OrgAdmission::Unmetered;
    };
    let Some(workspace_id) = workspace_id else {
        return OrgAdmission::ScopeMissing;
    };
    match admission
        .admit(OrgAdmissionRequest {
            workspace_id,
            operation,
            resource,
            operation_id,
            source: OrgRequestSource::Http,
        })
        .await
    {
        Ok(decision) if decision.allowed => OrgAdmission::Admitted(decision),
        Ok(decision) => OrgAdmission::Rejected(operation, decision),
        Err(error) => OrgAdmission::Unavailable(error),
    }
}

/// One protocol entry's Org admission: the shared authority and that
/// protocol's encoding.
#[derive(Clone)]
pub struct OrgAdmissionEdge {
    admission: Arc<dyn OrgRequestAdmission>,
    encoding: OrgAdmissionEncoding,
}

impl OrgAdmissionEdge {
    #[must_use]
    pub fn new(admission: Arc<dyn OrgRequestAdmission>, encoding: OrgAdmissionEncoding) -> Self {
        Self {
            admission,
            encoding,
        }
    }
}

/// Middleware: admit by Org bucket before the protocol handler runs. Mount it
/// inside the edge that stamps the trusted `WorkspaceScope`.
pub async fn admit_by_org_bucket(
    State(edge): State<OrgAdmissionEdge>,
    request: Request,
    next: Next,
) -> Response {
    let encoding = edge.encoding;
    let path = request.uri().path().to_owned();
    let class = (encoding.classify)(request.method(), &path);
    let workspace_id = request
        .extensions()
        .get::<awaken_runtime_contract::runs::scope::WorkspaceScope>()
        .and_then(awaken_runtime_contract::runs::scope::WorkspaceScope::non_empty)
        .map(str::to_owned);
    let operation_id = request
        .headers()
        .get("x-request-id")
        .or_else(|| request.headers().get("idempotency-key"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (mut response, decision) = match decide_org_admission(
        edge.admission.as_ref(),
        class,
        workspace_id,
        operation_id,
    )
    .await
    {
        OrgAdmission::Unmetered => return next.run(request).await,
        OrgAdmission::ScopeMissing => return (encoding.scope_missing)(&path),
        OrgAdmission::Unavailable(error) => return (encoding.unavailable)(&path, &error),
        OrgAdmission::Admitted(decision) => (next.run(request).await, decision),
        OrgAdmission::Rejected(operation, decision) => {
            ((encoding.rejected)(&path, operation, &decision), decision)
        }
    };
    if let Some(annotate) = encoding.annotate {
        annotate(response.headers_mut(), &decision);
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn token_buckets_are_independent_and_continuously_replenished() {
        const WALL_MS: u64 = 1_800_000_000_000;
        // Token-bucket decision table (capacity 2/minute per operation):
        // T1 first two creates -> admitted; T2 immediate third -> 429 decision;
        // T3 read at the exhausted create instant -> admitted (independent bucket);
        // T4 +30s -> exactly one create token replenished -> admitted.
        let limiter = OrgRateLimiter::with_limits(
            "org_test",
            OrgRateLimits {
                create_per_minute: 2,
                read_per_minute: 2,
            },
            awaken_time::system_clock(),
        );
        assert!(
            limiter
                .check_at(OrgOperation::Create, Duration::ZERO, WALL_MS)
                .allowed,
            "T1"
        );
        assert!(
            limiter
                .check_at(OrgOperation::Create, Duration::ZERO, WALL_MS)
                .allowed,
            "T1"
        );
        let rejected = limiter.check_at(OrgOperation::Create, Duration::ZERO, WALL_MS);
        assert!(!rejected.allowed, "T2");
        assert_eq!(rejected.retry_after, Some(30), "T2");
        assert_eq!(
            rejected.reset_at_unix_ms,
            WALL_MS + rejected.reset_after * 1_000,
            "T2 reset instant is the wall reading plus the refill time"
        );
        assert!(
            limiter
                .check_at(OrgOperation::Read, Duration::ZERO, WALL_MS)
                .allowed,
            "T3"
        );
        assert!(
            limiter
                .check_at(OrgOperation::Create, Duration::from_secs(30), WALL_MS)
                .allowed,
            "T4"
        );
    }

    #[test]
    #[should_panic(expected = "within the admitted product domain")]
    fn organization_bucket_rejects_capacity_above_the_proved_domain() {
        let _ = Bucket::new(MAX_ORGANIZATION_RATE_LIMIT + 1);
    }

    struct Unavailable;

    #[async_trait::async_trait]
    impl OrgRequestAdmission for Unavailable {
        async fn admit(
            &self,
            _request: OrgAdmissionRequest,
        ) -> Result<OrgAdmissionDecision, OrgAdmissionUnavailable> {
            Err(OrgAdmissionUnavailable {
                message: "store down".into(),
            })
        }
    }

    #[tokio::test]
    async fn org_admission_decision_table_fails_closed() {
        // Causes: C1 the protocol meters the route; C2 a trusted Workspace
        // scope exists; C3 the limiter answers; C4 a token is available.
        // | Rule | C1 | C2 | C3 | C4 | outcome |
        // | A0   | no | -  | -  | -  | Unmetered (limiter not consulted) |
        // | A1   | yes| no | -  | -  | ScopeMissing (limiter not consulted) |
        // | A2   | yes| yes| no | -  | Unavailable (refused, never admitted) |
        // | A3   | yes| yes| yes| yes| Admitted |
        // | A4   | yes| yes| yes| no | Rejected, with the operation |
        let limiter = OrgRateLimiter::with_limits(
            "org",
            OrgRateLimits {
                create_per_minute: 1,
                read_per_minute: 1,
            },
            awaken_time::system_clock(),
        );
        let class = Some((OrgOperation::Create, "sessions"));
        assert_eq!(
            decide_org_admission(&limiter, None, Some("ws".into()), None).await,
            OrgAdmission::Unmetered,
            "A0"
        );
        assert_eq!(
            decide_org_admission(&limiter, class, None, None).await,
            OrgAdmission::ScopeMissing,
            "A1"
        );
        assert!(
            matches!(
                decide_org_admission(&Unavailable, class, Some("ws".into()), None).await,
                OrgAdmission::Unavailable(_)
            ),
            "A2"
        );
        assert!(
            matches!(
                decide_org_admission(&limiter, class, Some("ws".into()), None).await,
                OrgAdmission::Admitted(_)
            ),
            "A3: the unconsulted A0/A1 rows left the token in place"
        );
        assert!(
            matches!(
                decide_org_admission(&limiter, class, Some("ws".into()), None).await,
                OrgAdmission::Rejected(OrgOperation::Create, decision) if !decision.allowed
            ),
            "A4"
        );
    }

    fn probe_classify(method: &Method, path: &str) -> Option<(OrgOperation, &'static str)> {
        match (method.as_str(), path) {
            ("POST", "/create") => Some((OrgOperation::Create, "probe")),
            ("GET", "/read") => Some((OrgOperation::Read, "probe")),
            _ => None,
        }
    }

    fn probe_annotate(headers: &mut HeaderMap, decision: &OrgAdmissionDecision) {
        headers.insert("x-remaining", decision.remaining.into());
    }

    const PROBE: OrgAdmissionEncoding = OrgAdmissionEncoding {
        classify: probe_classify,
        scope_missing: |_| (StatusCode::SERVICE_UNAVAILABLE, "scope").into_response(),
        unavailable: |_, error| {
            (StatusCode::SERVICE_UNAVAILABLE, error.message.clone()).into_response()
        },
        rejected: |_, _, _| (StatusCode::TOO_MANY_REQUESTS, "probe-rejected").into_response(),
        annotate: Some(probe_annotate),
    };

    #[tokio::test]
    async fn the_edge_runs_the_handler_only_for_unmetered_or_admitted_requests() {
        // Effects per outcome: E1 unmetered runs the handler unannotated; E2
        // admitted runs it and the protocol annotates; E3 rejected never runs
        // it, answers the protocol's own rejection, annotated; E4 a missing
        // scope answers the protocol's own refusal.
        let limiter: Arc<dyn OrgRequestAdmission> = Arc::new(OrgRateLimiter::with_limits(
            "org",
            OrgRateLimits {
                create_per_minute: 1,
                read_per_minute: 1,
            },
            awaken_time::system_clock(),
        ));
        let app = Router::new()
            .route("/create", post(|| async { "created" }))
            .route("/free", post(|| async { "free" }))
            .layer(axum::middleware::from_fn_with_state(
                OrgAdmissionEdge::new(limiter.clone(), PROBE),
                admit_by_org_bucket,
            ));
        let scoped = app.clone().layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope("ws".into()),
        ));
        let call =
            |app: Router, uri: &str| app.oneshot(Request::post(uri).body(Body::empty()).unwrap());
        let free = call(scoped.clone(), "/free").await.unwrap();
        assert_eq!(free.status(), StatusCode::OK, "E1");
        assert!(free.headers().get("x-remaining").is_none(), "E1");
        let admitted = call(scoped.clone(), "/create").await.unwrap();
        assert_eq!(admitted.status(), StatusCode::OK, "E2");
        assert_eq!(admitted.headers()["x-remaining"], "0", "E2");
        let rejected = call(scoped, "/create").await.unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS, "E3");
        assert_eq!(rejected.headers()["x-remaining"], "0", "E3");
        let unscoped = call(app, "/create").await.unwrap();
        assert_eq!(unscoped.status(), StatusCode::SERVICE_UNAVAILABLE, "E4");
    }

    #[tokio::test]
    async fn plain_text_org_admission_keeps_the_existing_http_error_shape() {
        let encoding = OrgAdmissionEncoding::plain_text(probe_classify);

        // Causes: C1 missing trusted scope; C2 unavailable limiter; C3 refused
        // Create/Read bucket; C4 Retry-After present/absent. All errors retain
        // the selected plain-text body and Content-Type, not only its status.
        // | Rule | Cause       | Status | Body                         | Retry |
        // | E1   | C1          | 503    | exact missing-scope message  | none  |
        // | E2   | C2          | 503    | exact limiter error          | none  |
        // | E3   | C3 x C4     | 429    | exact operation-specific text| C4    |
        // E3 crosses both operations with both optional-header branches.
        let mut cases = vec![
            (
                (encoding.scope_missing)("/create"),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                "request has no trusted Workspace scope",
            ),
            (
                (encoding.unavailable)(
                    "/create",
                    &OrgAdmissionUnavailable {
                        message: "down".into(),
                    },
                ),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                "Org request limiter is unavailable: down",
            ),
        ];
        for (operation, body) in [
            (OrgOperation::Create, "Org Create request limit exceeded"),
            (OrgOperation::Read, "Org Read request limit exceeded"),
        ] {
            for retry_after in [Some(20), None] {
                let decision = OrgAdmissionDecision {
                    allowed: false,
                    limit: 3,
                    remaining: 0,
                    retry_after,
                    reset_after: 60,
                    reset_at_unix_ms: 0,
                };
                cases.push((
                    (encoding.rejected)("/create", operation, &decision),
                    StatusCode::TOO_MANY_REQUESTS,
                    retry_after.map(|_| "20"),
                    body,
                ));
            }
        }
        for (response, status, retry_after, body) in cases {
            assert_eq!(response.status(), status, "{body}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8",
                "{body}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .map(|value| value.to_str().unwrap()),
                retry_after,
                "{body}",
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), body.as_bytes(), "exact public error body");
        }
    }
}
