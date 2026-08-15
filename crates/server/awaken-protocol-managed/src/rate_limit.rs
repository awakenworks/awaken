//! Organization-scoped request limiting for the Managed Agents HTTP surface.
//!
//! The edge supplies the authenticated Workspace coordinate. Open/local
//! deployments may map every Workspace to their one configured organization;
//! hosted adapters resolve it through their IAM tenant authority before taking
//! an organization bucket.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::types::ErrorResponse;

const MILLIS_PER_SECOND: u64 = 1_000;
const TOKEN_QUANTA_PER_TOKEN: u64 = 60 * MILLIS_PER_SECOND;
const TOKEN_QUANTA_PER_TOKEN_U32: u32 = TOKEN_QUANTA_PER_TOKEN as u32;
/// Explicit product admission bound, over 54 times the documented 1,200/minute
/// read limit. It keeps exact fixed-point arithmetic and its proof domain finite.
const MAX_ORGANIZATION_RATE_LIMIT: u32 = u16::MAX as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedOperation {
    Create,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedRequestSource {
    Http,
    Deployment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRateLimitRequest {
    pub workspace_id: String,
    pub operation: ManagedOperation,
    pub resource: &'static str,
    pub operation_id: Option<String>,
    pub source: ManagedRequestSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedRateLimitDecision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub retry_after: Option<u64>,
    pub reset_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Managed request limiter is unavailable: {message}")]
pub struct ManagedRateLimitUnavailable {
    pub message: String,
}

#[async_trait::async_trait]
pub trait ManagedRequestLimiter: Send + Sync {
    async fn admit(
        &self,
        request: ManagedRateLimitRequest,
    ) -> Result<ManagedRateLimitDecision, ManagedRateLimitUnavailable>;
}

/// Anthropic's documented organization-level Managed Agents defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedRateLimits {
    pub create_per_minute: u32,
    pub read_per_minute: u32,
}

impl Default for ManagedRateLimits {
    fn default() -> Self {
        Self {
            create_per_minute: 300,
            read_per_minute: 1_200,
        }
    }
}

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
            "a Managed API rate limit must be within the admitted product domain"
        );
        let capacity = u16::try_from(capacity).expect("admission bound fits u16");
        Self {
            capacity,
            token_quanta: capacity_quanta(capacity),
            last_refill: Duration::ZERO,
        }
    }

    fn take(&mut self, now: Duration) -> ManagedRateLimitDecision {
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
        ManagedRateLimitDecision {
            allowed,
            limit: u32::from(self.capacity),
            remaining: (self.token_quanta / TOKEN_QUANTA_PER_TOKEN) as u32,
            retry_after,
            reset_after: seconds_for_quanta(
                capacity_quanta(self.capacity) - self.token_quanta,
                self.capacity,
            ),
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

/// Shared limiter for one organization-serving router.
#[derive(Debug)]
pub struct ManagedRateLimiter {
    organization_id: String,
    started_at: Instant,
    buckets: Mutex<Buckets>,
}

impl ManagedRateLimiter {
    #[must_use]
    pub fn for_organization(organization_id: impl Into<String>) -> Self {
        Self::with_limits(organization_id, ManagedRateLimits::default())
    }

    #[must_use]
    pub fn with_limits(organization_id: impl Into<String>, limits: ManagedRateLimits) -> Self {
        Self {
            organization_id: organization_id.into(),
            started_at: Instant::now(),
            buckets: Mutex::new(Buckets {
                create: Bucket::new(limits.create_per_minute),
                read: Bucket::new(limits.read_per_minute),
            }),
        }
    }

    fn check(&self, operation: ManagedOperation) -> ManagedRateLimitDecision {
        self.check_at(operation, self.started_at.elapsed())
    }

    /// Admit a Session created internally by a Deployment. Scheduled/manual
    /// deployment launches bypass HTTP but consume the same organization Create
    /// bucket as `POST /v1/sessions`.
    fn check_at(&self, operation: ManagedOperation, now: Duration) -> ManagedRateLimitDecision {
        let mut buckets = self.buckets.lock().unwrap();
        match operation {
            ManagedOperation::Create => buckets.create.take(now),
            ManagedOperation::Read => buckets.read.take(now),
        }
    }

    #[must_use]
    pub fn organization_id(&self) -> &str {
        &self.organization_id
    }
}

#[async_trait::async_trait]
impl ManagedRequestLimiter for ManagedRateLimiter {
    async fn admit(
        &self,
        request: ManagedRateLimitRequest,
    ) -> Result<ManagedRateLimitDecision, ManagedRateLimitUnavailable> {
        Ok(self.check(request.operation))
    }
}

/// Enforce the two documented Managed Agents request buckets. Non-Managed routes
/// and Managed mutation endpoints that are neither Create nor Read pass through.
pub async fn enforce_managed_rate_limit(
    State(limiter): State<std::sync::Arc<dyn ManagedRequestLimiter>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let Some((operation, resource)) = classify(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let Some(workspace_id) = request
        .extensions()
        .get::<awaken_tenancy::WorkspaceScope>()
        .and_then(awaken_tenancy::WorkspaceScope::non_empty)
        .map(str::to_owned)
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "api_error",
                "Managed request has no trusted Workspace scope",
            )),
        )
            .into_response();
    };
    let operation_id = request
        .headers()
        .get("x-request-id")
        .or_else(|| request.headers().get("idempotency-key"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let decision = match limiter
        .admit(ManagedRateLimitRequest {
            workspace_id,
            operation,
            resource,
            operation_id,
            source: ManagedRequestSource::Http,
        })
        .await
    {
        Ok(decision) => decision,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new("api_error", error.to_string())),
            )
                .into_response();
        }
    };
    let mut response = if decision.allowed {
        next.run(request).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                format!("Managed Agents {operation:?} request limit exceeded"),
            )),
        )
            .into_response()
    };
    apply_headers(response.headers_mut(), decision);
    response
}

fn apply_headers(headers: &mut HeaderMap, decision: ManagedRateLimitDecision) {
    insert_header(
        headers,
        "anthropic-ratelimit-requests-limit",
        u64::from(decision.limit),
    );
    insert_header(
        headers,
        "anthropic-ratelimit-requests-remaining",
        u64::from(decision.remaining),
    );
    let reset_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + decision.reset_after.saturating_mul(1_000);
    if let Ok(value) =
        HeaderValue::from_str(&awaken_session_contract::epoch_millis_to_rfc3339(reset_ms))
    {
        headers.insert("anthropic-ratelimit-requests-reset", value);
    }
    if let Some(retry_after) = decision.retry_after {
        insert_header(headers, "retry-after", retry_after);
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(name, value);
    }
}

fn classify(method: &Method, path: &str) -> Option<(ManagedOperation, &'static str)> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let resource = managed_family(&segments)?;
    if matches!(*method, Method::GET | Method::HEAD) {
        return Some((ManagedOperation::Read, resource));
    }
    (*method == Method::POST && is_create_endpoint(&segments))
        .then_some((ManagedOperation::Create, resource))
}

fn managed_family(segments: &[&str]) -> Option<&'static str> {
    if segments.first() != Some(&"v1") {
        return None;
    }
    match segments.get(1).copied()? {
        "agents" => Some("agents"),
        "sessions" => Some("sessions"),
        "environments" => Some("environments"),
        "deployments" => Some("deployments"),
        "deployment_runs" => Some("deployment_runs"),
        "vaults" => Some("vaults"),
        "memory_stores" => Some("memory_stores"),
        "skills" => Some("skills"),
        "user_profiles" => Some("user_profiles"),
        "dreams" => Some("dreams"),
        "files" => Some("files"),
        "models" => Some("models"),
        "tunnels" => Some("tunnels"),
        _ => None,
    }
}

fn is_create_endpoint(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["v1", "agents"]
            | ["v1", "sessions"]
            | ["v1", "environments"]
            | ["v1", "deployments"]
            | ["v1", "vaults"]
            | ["v1", "memory_stores"]
            | ["v1", "skills"]
            | ["v1", "user_profiles"]
            | ["v1", "dreams"]
            | ["v1", "sessions", _, "resources"]
            | ["v1", "vaults", _, "credentials"]
            | ["v1", "memory_stores", _, "memories"]
            | ["v1", "skills", _, "versions"]
            | ["v1", "user_profiles", _, "enrollment_url"]
            | ["v1", "files"]
            | ["v1", "tunnels"]
            | ["v1", "tunnels", _, "certificates"]
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn endpoint_classification_is_generated_from_the_operation_decision_table() {
        // Cause/effect graph:
        // C1 known Managed family; C2 GET/HEAD; C3 POST names an API operation
        // whose documented verb is Create. C1&C2 -> read bucket; C1&C3 -> create
        // bucket; every other combination -> no Managed request bucket.
        //
        // | Rule | family | method/operation | effect |
        // |---|---|---|---|
        // | C1 | Managed | GET/HEAD retrieve/list/stream | Read |
        // | C2 | Managed | POST exact collection/nested Create | Create |
        // | C3 | Managed | update/archive/action/delete | pass through |
        // | C4 | unknown extension or non-Managed | any | pass through |
        for path in [
            "/v1/agents",
            "/v1/sessions",
            "/v1/environments",
            "/v1/deployments",
            "/v1/vaults",
            "/v1/memory_stores",
            "/v1/skills",
            "/v1/user_profiles",
            "/v1/dreams",
            "/v1/sessions/ses_1/resources",
            "/v1/vaults/vlt_1/credentials",
            "/v1/memory_stores/mem_1/memories",
            "/v1/skills/sk_1/versions",
            "/v1/user_profiles/usr_1/enrollment_url",
            "/v1/files",
            "/v1/tunnels",
            "/v1/tunnels/tnl_1/certificates",
        ] {
            assert_eq!(
                classify(&Method::POST, path),
                Some((
                    ManagedOperation::Create,
                    path.trim_start_matches("/v1/").split('/').next().unwrap()
                )),
                "C2 {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                classify(&method, "/v1/sessions/ses_1/events/stream"),
                Some((ManagedOperation::Read, "sessions")),
                "C1"
            );
        }
        for path in [
            "/v1/agents/a_1/archive",
            "/v1/sessions/s_1/events",
            "/v1/deployments/d_1/run",
            "/v1/vaults/v_1",
        ] {
            assert_eq!(classify(&Method::POST, path), None, "C3 {path}");
        }
        for path in ["/v1/extensions", "/healthz"] {
            assert_eq!(classify(&Method::GET, path), None, "C4 {path}");
        }
        assert_eq!(
            classify(&Method::GET, "/v1/models"),
            Some((ManagedOperation::Read, "models")),
            "C1 models"
        );
    }

    #[test]
    fn token_buckets_are_independent_and_continuously_replenished() {
        // Token-bucket decision table (capacity 2/minute per operation):
        // T1 first two creates -> admitted; T2 immediate third -> 429 decision;
        // T3 read at the exhausted create instant -> admitted (independent bucket);
        // T4 +30s -> exactly one create token replenished -> admitted.
        let limiter = ManagedRateLimiter::with_limits(
            "org_test",
            ManagedRateLimits {
                create_per_minute: 2,
                read_per_minute: 2,
            },
        );
        assert!(
            limiter
                .check_at(ManagedOperation::Create, Duration::ZERO)
                .allowed,
            "T1"
        );
        assert!(
            limiter
                .check_at(ManagedOperation::Create, Duration::ZERO)
                .allowed,
            "T1"
        );
        let rejected = limiter.check_at(ManagedOperation::Create, Duration::ZERO);
        assert!(!rejected.allowed, "T2");
        assert_eq!(rejected.retry_after, Some(30), "T2");
        assert!(
            limiter
                .check_at(ManagedOperation::Read, Duration::ZERO)
                .allowed,
            "T3"
        );
        assert!(
            limiter
                .check_at(ManagedOperation::Create, Duration::from_secs(30))
                .allowed,
            "T4"
        );
    }

    #[test]
    #[should_panic(expected = "within the admitted product domain")]
    fn organization_bucket_rejects_capacity_above_the_proved_domain() {
        let _ = Bucket::new(MAX_ORGANIZATION_RATE_LIMIT + 1);
    }

    #[tokio::test]
    async fn classified_request_without_trusted_workspace_fails_before_admission() {
        // Causes: C1 request classifies as Managed Create; C2 no edge-authored
        // WorkspaceScope exists. Effects: E1 503; E2 handler does not run; E3
        // limiter is not given a caller-invented fallback tenant. Decision rule
        // A1=C1+C2 -> E1+E2+E3. The scoped success and shared-bucket rules are
        // exercised by the following compatibility test.
        let limiter: Arc<dyn ManagedRequestLimiter> =
            Arc::new(ManagedRateLimiter::for_organization("org"));
        let app = Router::new()
            .route("/v1/sessions", post(|| async { StatusCode::CREATED }))
            .layer(axum::middleware::from_fn_with_state(
                limiter,
                enforce_managed_rate_limit,
            ));
        let response = app
            .oneshot(Request::post("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "A1");
    }

    #[tokio::test]
    async fn native_and_acp_session_creates_share_the_organization_bucket() {
        // Runtime-axis cause graph: Native and ACP differ only after Session
        // creation. Rate limiting is an organization ingress effect, so N1 Native
        // create and N2 ACP create consume the same capacity; N3 any third create
        // is rejected before its handler. The read bucket remains independently
        // available and every metered response exposes limit state.
        async fn accepted(body: String) -> String {
            body
        }
        let limiter: Arc<dyn ManagedRequestLimiter> = Arc::new(ManagedRateLimiter::with_limits(
            "org_shared",
            ManagedRateLimits {
                create_per_minute: 2,
                read_per_minute: 1,
            },
        ));
        let app = Router::new()
            .route("/v1/sessions", post(accepted).get(|| async { "sessions" }))
            .layer(axum::middleware::from_fn_with_state(
                limiter,
                enforce_managed_rate_limit,
            ))
            .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
                "workspace_shared".into(),
            )));
        for (rule, agent) in [("N1", "native:claude"), ("N2", "acp:claude-code")] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/sessions")
                        .body(Body::from(agent))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
        }
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/sessions")
                    .body(Body::from("native:other"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "N3");
        assert_eq!(response.headers()["retry-after"], "30", "N3");
        assert_eq!(
            response.headers()["anthropic-ratelimit-requests-limit"],
            "2",
            "N3"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["type"], "rate_limit_error", "N3");

        let read = app
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::OK, "independent read bucket");
        assert_eq!(read.headers()["anthropic-ratelimit-requests-limit"], "1");
    }
}
