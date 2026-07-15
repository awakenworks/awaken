//! Request enforcement for the agent runtime's HTTP surface — the open
//! "judgment" half of access control.
//!
//! It does four things and nothing else: (1) authenticate a presented bearer
//! credential against an in-memory [`ApiTokenDirectory`], (2) derive the
//! request's authorization scope — every request anchors at its
//! [`ScopeRef::Workspace`] (tenancy is strictly Org → Workspace), (3) map the
//! route to an [`ActionKey`], and (4) authorize via the same default-deny
//! [`PolicySet`] engine every Awaken product shares. It is in-memory and seeded,
//! so the single-machine standalone needs no durable IAM store — that (minting
//! HTTP surface, `awaken-iam-server` persistence, multi-tenant provisioning) is
//! the authoring half and lives elsewhere.
//!
//! Scope fencing is free: a token whose `RoleBinding` sits at
//! `Workspace{ws_local}` cannot reach `Workspace{ws_other}`, because the scope
//! graph resolves that up through `Global`, never to `ws_local`. So an
//! out-of-tenant request is denied even for an `admin` token.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use awaken_iam_contract::{
    ActionKey, AuthorizationDecision, AuthorizationRequest, PrincipalRef, ScopeRef, Timestamp,
    WorkspaceId,
};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, Effect, Grant, GrantId, GrantSubject, IamError,
    MintApiToken, OsEntropy, PolicySet, RoleId,
};
use awaken_iam_preset::named_role_catalog;
use awaken_tenancy::{Authority, ScopeClaim, ScopeId, resolve_scope};

/// The managed-agent / session surface's action namespace. The preset role
/// catalog grants `workspace.*` / `apikey.* / file.* / skill.*` but NOT this, so
/// [`EnforceEngine::seeded`] installs an `agent.*` grant for the `admin` role
/// (the model forbids a literal `*` pattern in a role, so it is granted here).
const AGENT_NAMESPACE: &str = "agent";

/// A request to mint an in-memory service token.
pub struct TokenSpec {
    /// Stable, unique token row id.
    pub token_id: String,
    /// Service principal the token authenticates as.
    pub service_id: String,
    /// Workspace the token's role binding sits at (its tenant fence).
    pub workspace_id: String,
    /// Preset role the principal holds at the workspace (`admin`, …).
    pub role: String,
    /// Optional RFC 3339 expiry (strictly after mint time).
    pub expires_at: Option<String>,
}

/// In-memory enforcement engine: a token directory + a policy, seeded with the
/// preset role catalog. Cheap to construct; the single-machine seeder mints a
/// couple of tokens into it at boot.
pub struct EnforceEngine {
    state: Mutex<EngineState>,
}

struct EngineState {
    directory: ApiTokenDirectory,
    policy: PolicySet,
}

impl EnforceEngine {
    /// A fresh engine with the preset Anthropic role catalog installed as
    /// Global `GrantSubject::Role` grants, plus an `agent.*` grant for `admin`
    /// so the managed-agent surface is reachable. A role is only *held* where a
    /// principal's `RoleBinding` covers, so Global grants are not wildcard
    /// authority — the binding confines the reach.
    #[must_use]
    pub fn seeded() -> Self {
        let now = Timestamp(now_rfc3339());
        let mut policy = PolicySet::new();
        for role in named_role_catalog(&now) {
            for (index, pattern) in role.action_patterns.iter().enumerate() {
                policy.add_grant(Grant {
                    id: GrantId(format!("role:{}:{index}", role.id.0)),
                    subject: GrantSubject::Role(role.id.clone()),
                    action_pattern: pattern.clone(),
                    scope: ScopeRef::Global,
                    effect: Effect::Allow,
                });
            }
        }
        // The managed-agent surface: grant `agent.*` to `admin` (the single
        // namespace the preset catalog omits, and which a role may not carry as
        // a bare `*`).
        policy.add_grant(Grant {
            id: GrantId("role:admin:agent".to_string()),
            subject: GrantSubject::Role(RoleId("admin".to_string())),
            action_pattern: awaken_iam_core::ActionPattern(format!("{AGENT_NAMESPACE}.*")),
            scope: ScopeRef::Global,
            effect: Effect::Allow,
        });
        Self {
            state: Mutex::new(EngineState {
                directory: ApiTokenDirectory::new(),
                policy,
            }),
        }
    }

    /// Mint a service token bound to `role` at `workspace` (in-memory only).
    /// Returns the one-time cleartext `sk-awaken-…` secret.
    pub fn mint(&self, spec: TokenSpec) -> Result<String, IamError> {
        let principal = PrincipalRef::Service {
            service_id: spec.service_id,
        };
        let request = MintApiToken {
            id: awaken_iam_contract::ApiTokenId(spec.token_id),
            principal,
            workspace: WorkspaceId(spec.workspace_id),
            role: RoleId(spec.role),
            created_at: Timestamp(now_rfc3339()),
            expires_at: spec.expires_at.map(Timestamp),
        };
        let mut guard = self.state.lock().expect("enforce engine poisoned");
        let state = &mut *guard;
        let issued = ApiTokenMinter::new(OsEntropy).mint(
            &mut state.directory,
            &mut state.policy,
            request,
        )?;
        Ok(issued.secret)
    }

    /// Authenticate a presented bearer credential → its principal and the
    /// workspace its role binding sits at. `Err` on unknown/expired/revoked.
    pub fn authenticate(&self, presented: &str) -> Result<(PrincipalRef, WorkspaceId), IamError> {
        let guard = self.state.lock().expect("enforce engine poisoned");
        let token = guard
            .directory
            .authenticate(presented, &Timestamp(now_rfc3339()))?;
        Ok((token.principal.clone(), token.workspace.clone()))
    }

    /// Authorize `principal` performing `action` at `scope`. Fail-closed: an
    /// unmatched request is [`AuthorizationDecision::Deny`].
    pub fn authorize(
        &self,
        principal: PrincipalRef,
        action: &ActionKey,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        let request = AuthorizationRequest::direct(principal, action.clone(), scope);
        self.state
            .lock()
            .expect("enforce engine poisoned")
            .policy
            .evaluate(&request)
            .decision
    }
}

/// Derive the authorization scope from a request's tenancy. Tenancy is strictly
/// Org → Workspace: every request anchors at its [`ScopeRef::Workspace`] (the
/// token's home workspace). Project is not a tenancy tier.
#[must_use]
pub fn request_scope(workspace_id: &str) -> ScopeRef {
    ScopeRef::Workspace {
        workspace_id: WorkspaceId(workspace_id.to_string()),
    }
}

/// Map a session/protocol route to its action: reads (`GET`/`HEAD`) →
/// `agent.read`, every mutation → `agent.write`. Both live under the `agent.*`
/// namespace granted to a runtime-capable role.
#[must_use]
pub fn session_action(method: &str) -> ActionKey {
    match method {
        "GET" | "HEAD" => ActionKey(format!("{AGENT_NAMESPACE}.read")),
        _ => ActionKey(format!("{AGENT_NAMESPACE}.write")),
    }
}

/// The tenancy the ingress resolved for a request, stamped into the request
/// extensions before the [`guard`] runs. Absent for a bare request, where the
/// guard falls back to the token's home workspace.
#[derive(Debug, Clone)]
pub struct RequestTenancy {
    pub workspace_id: String,
}

/// The session-axis guard: authenticate the bearer, derive the scope from the
/// request's [`RequestTenancy`] (or the token's workspace when bare), map the
/// method to an action, and authorize — fail-closed. Apply with
/// `axum::middleware::from_fn_with_state(engine, guard)` over the protocol
/// router. A missing/invalid credential is 401; a denied decision is 403.
pub async fn guard(
    State(engine): State<Arc<EnforceEngine>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(presented) = presented_bearer(request.headers()) else {
        return reject(StatusCode::UNAUTHORIZED, "missing bearer credential");
    };
    let Ok((principal, token_workspace)) = engine.authenticate(&presented) else {
        return reject(StatusCode::UNAUTHORIZED, "invalid credential");
    };
    // Resolve one authorized scope from the ingress claims (ADR-0051 D4). The
    // token is authority — enforce-engine tokens are narrow, bound to one
    // workspace — and a `RequestTenancy` stamped upstream is a path/domain
    // *selection*. `resolve_scope` applies the fence: a selection the token does
    // not cover is rejected here, before the policy engine is consulted, and it
    // can never widen the token's reach.
    let token_scope = ScopeId::from(token_workspace.0.as_str());
    let authority = Authority::bound(token_scope.clone());
    let mut claims = vec![ScopeClaim::FromToken(token_scope)];
    if let Some(tenancy) = request.extensions().get::<RequestTenancy>() {
        claims.push(ScopeClaim::FromPath(tenancy.workspace_id.clone()));
    }
    let workspace_id = match resolve_scope(&authority, &claims) {
        Ok(scope) => scope.0,
        Err(_) => return reject(StatusCode::FORBIDDEN, "not authorized for this scope"),
    };
    let scope = request_scope(&workspace_id);
    let action = session_action(request.method().as_str());
    match engine.authorize(principal, &action, scope) {
        AuthorizationDecision::Allow => {
            // Publish the edge-resolved owning workspace so a downstream projection
            // (webhooks/usage) can stamp it — the aspect resolves tenancy, the core
            // never stores it. Idempotent: a project-less deployment has no prior
            // `RequestTenancy`, a future one would already carry the same value.
            request
                .extensions_mut()
                .insert(RequestTenancy { workspace_id });
            next.run(request).await
        }
        AuthorizationDecision::Deny | AuthorizationDecision::RequireApproval => {
            reject(StatusCode::FORBIDDEN, "not authorized for this scope")
        }
    }
}

/// The presented secret from `Authorization: Bearer …` or the SDK's `x-api-key`.
fn presented_bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        let trimmed = value.trim();
        if let Some(rest) = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
        {
            return Some(rest.trim().to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn reject(status: StatusCode, detail: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": { "type": "unauthorized", "message": detail } })),
    )
        .into_response()
}

/// Days since the Unix epoch → RFC 3339 UTC, allocation-free of any date crate
/// (Howard Hinnant's public-domain `civil_from_days`).
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "wrkspc_local";

    fn engine_with_admin() -> (EnforceEngine, String, PrincipalRef) {
        let engine = EnforceEngine::seeded();
        let secret = engine
            .mint(TokenSpec {
                token_id: "tok_admin".into(),
                service_id: "operator".into(),
                workspace_id: WS.into(),
                role: "admin".into(),
                expires_at: None,
            })
            .expect("mint admin");
        let (principal, ws) = engine.authenticate(&secret).expect("authenticate");
        assert_eq!(ws.0, WS);
        (engine, secret, principal)
    }

    #[test]
    fn seeded_engine_mints_and_authenticates() {
        let (_engine, secret, principal) = engine_with_admin();
        assert!(
            secret.starts_with("sk-awaken-"),
            "cleartext is Awaken-branded (sk-awaken-), distinct from provider keys"
        );
        assert!(matches!(principal, PrincipalRef::Service { .. }));
    }

    #[test]
    fn an_unknown_token_is_rejected() {
        let engine = EnforceEngine::seeded();
        assert!(
            engine
                .authenticate("sk-ant-nope.definitely-not-a-token")
                .is_err()
        );
    }

    #[test]
    fn admin_is_authorized_at_its_workspace() {
        let (engine, _secret, principal) = engine_with_admin();
        let write = engine.authorize(
            principal.clone(),
            &session_action("POST"),
            request_scope(WS),
        );
        assert_eq!(write, AuthorizationDecision::Allow);
        let read = engine.authorize(principal, &session_action("GET"), request_scope(WS));
        assert_eq!(read, AuthorizationDecision::Allow);
    }

    #[test]
    fn another_workspace_is_denied_even_for_admin() {
        let (engine, _secret, principal) = engine_with_admin();
        let cross = engine.authorize(
            principal,
            &session_action("POST"),
            request_scope("wrkspc_other"),
        );
        assert_eq!(cross, AuthorizationDecision::Deny, "the scope fence holds");
    }

    #[test]
    fn request_scope_anchors_at_the_workspace() {
        assert!(matches!(request_scope(WS), ScopeRef::Workspace { .. }));
    }

    #[test]
    fn session_action_maps_reads_and_writes() {
        assert_eq!(session_action("GET").0, "agent.read");
        assert_eq!(session_action("POST").0, "agent.write");
        assert_eq!(session_action("DELETE").0, "agent.write");
    }

    // --- guard middleware (session-axis, deployment-form-independent) ---------

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::any;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn call(
        engine: Arc<EnforceEngine>,
        method: Method,
        bearer: Option<&str>,
        tenancy: Option<RequestTenancy>,
    ) -> StatusCode {
        let app = Router::new()
            .fallback(any(|| async { "ok" }))
            .layer(from_fn_with_state(engine, guard));
        let mut builder = Request::builder().method(method).uri("/v1/sessions");
        if let Some(bearer) = bearer {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        let mut request = builder.body(Body::empty()).expect("request");
        if let Some(tenancy) = tenancy {
            request.extensions_mut().insert(tenancy);
        }
        app.oneshot(request).await.expect("router call").status()
    }

    fn admin_engine() -> (Arc<EnforceEngine>, String) {
        let engine = Arc::new(EnforceEngine::seeded());
        let secret = engine
            .mint(TokenSpec {
                token_id: "tok_admin".into(),
                service_id: "operator".into(),
                workspace_id: WS.into(),
                role: "admin".into(),
                expires_at: None,
            })
            .expect("mint admin");
        (engine, secret)
    }

    #[tokio::test]
    async fn guard_rejects_a_missing_credential() {
        let (engine, _s) = admin_engine();
        assert_eq!(
            call(engine, Method::POST, None, None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn guard_rejects_an_invalid_credential() {
        let (engine, _s) = admin_engine();
        assert_eq!(
            call(engine, Method::GET, Some("sk-ant-bogus.nope"), None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn guard_allows_a_bare_request_with_a_valid_token() {
        let (engine, secret) = admin_engine();
        assert_eq!(
            call(engine, Method::POST, Some(&secret), None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn guard_allows_the_tokens_workspace() {
        let (engine, secret) = admin_engine();
        let tenancy = RequestTenancy {
            workspace_id: WS.into(),
        };
        assert_eq!(
            call(engine, Method::POST, Some(&secret), Some(tenancy)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn guard_forbids_another_workspace() {
        let (engine, secret) = admin_engine();
        let tenancy = RequestTenancy {
            workspace_id: "wrkspc_other".into(),
        };
        assert_eq!(
            call(engine, Method::POST, Some(&secret), Some(tenancy)).await,
            StatusCode::FORBIDDEN
        );
    }

    // G3 (write-back clause): an authorized bare request has its owning
    // workspace stamped into the request extensions for downstream projection.
    #[tokio::test]
    async fn guard_stamps_the_owning_workspace_on_a_bare_request() {
        let (engine, secret) = admin_engine();
        let app = Router::new()
            .fallback(any(
                |axum::Extension(tenancy): axum::Extension<RequestTenancy>| async move {
                    tenancy.workspace_id
                },
            ))
            .layer(from_fn_with_state(engine, guard));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/sessions")
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("router call");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            &body[..],
            WS.as_bytes(),
            "guard writes back the token's workspace"
        );
    }

    // Mint a token bound to an arbitrary preset role at the local workspace.
    fn engine_with_role(role: &str) -> (Arc<EnforceEngine>, String) {
        let engine = Arc::new(EnforceEngine::seeded());
        let secret = engine
            .mint(TokenSpec {
                token_id: "tok_role".into(),
                service_id: "operator".into(),
                workspace_id: WS.into(),
                role: role.into(),
                expires_at: None,
            })
            .expect("mint role token");
        (engine, secret)
    }

    // G6: a valid token whose role carries no `agent.*` grant is scope-clean at
    // its own workspace but denied at the policy engine → 403. The preset
    // `developer` role holds `apikey.*` etc. but never the managed-agent verbs.
    #[tokio::test]
    async fn guard_forbids_a_role_without_the_agent_grant() {
        let (engine, secret) = engine_with_role("developer");
        assert_eq!(
            call(engine, Method::POST, Some(&secret), None).await,
            StatusCode::FORBIDDEN,
        );
    }

    // --- presented_bearer (F5) -----------------------------------------------

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                axum::http::HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    // B1: Authorization: Bearer sk-x → Some("sk-x").
    #[test]
    fn presented_bearer_reads_the_authorization_bearer() {
        assert_eq!(
            presented_bearer(&header_map(&[("authorization", "Bearer sk-x")])),
            Some("sk-x".to_string())
        );
    }

    // B2: only x-api-key → Some("sk-y").
    #[test]
    fn presented_bearer_falls_back_to_x_api_key() {
        assert_eq!(
            presented_bearer(&header_map(&[("x-api-key", "sk-y")])),
            Some("sk-y".to_string())
        );
    }

    // B3: non-Bearer Authorization + x-api-key → the x-api-key value.
    #[test]
    fn presented_bearer_ignores_non_bearer_and_uses_x_api_key() {
        assert_eq!(
            presented_bearer(&header_map(&[
                ("authorization", "Basic dXNlcjpwYXNz"),
                ("x-api-key", "sk-y"),
            ])),
            Some("sk-y".to_string())
        );
    }

    // B4: neither header → None.
    #[test]
    fn presented_bearer_absent_is_none() {
        assert_eq!(presented_bearer(&header_map(&[])), None);
    }

    // B5: empty x-api-key is filtered → None.
    #[test]
    fn presented_bearer_filters_an_empty_x_api_key() {
        assert_eq!(presented_bearer(&header_map(&[("x-api-key", "")])), None);
    }

    // B6: a present-but-empty `"Bearer "` header. `value.trim()` strips the
    // delimiter space, so the `"Bearer "` prefix no longer matches and the value
    // falls through to `None`. NOTE (latent inconsistency, not fixed here): this
    // means a present-but-empty bearer is reported as a *missing* credential (401
    // "missing bearer credential") rather than an *invalid* one — both are 401, so
    // no behavior/security impact, but the message is arguably misleading. Pinned
    // as current behavior; flagged for a deliberate product decision, not smuggled
    // in via a test change.
    #[test]
    fn presented_bearer_empty_bearer_falls_through_to_none() {
        assert_eq!(
            presented_bearer(&header_map(&[("authorization", "Bearer ")])),
            None
        );
    }

    // --- authenticate / session_action (F4/F6) -------------------------------

    // A6: an expired token fails authentication. `mint` fail-closes on a past
    // expiry (InvalidApiTokenWindow), so an already-expired token is not
    // constructible directly — mint a short-lived one and let it lapse.
    fn rfc3339_secs_from_now(delta: u64) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + delta;
        let (year, month, day) = civil_from_days((secs / 86_400) as i64);
        let rem = secs % 86_400;
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        )
    }

    #[test]
    fn authenticate_rejects_an_expired_token() {
        let engine = EnforceEngine::seeded();
        let secret = engine
            .mint(TokenSpec {
                token_id: "tok_exp".into(),
                service_id: "operator".into(),
                workspace_id: WS.into(),
                role: "admin".into(),
                expires_at: Some(rfc3339_secs_from_now(2)),
            })
            .expect("mint short-lived token");
        std::thread::sleep(std::time::Duration::from_secs(3));
        match engine.authenticate(&secret) {
            Err(IamError::ApiTokenExpired { .. }) => {}
            other => panic!("expected an expired-token error, got {other:?}"),
        }
    }

    // A10: session_action is case-sensitive — only uppercase GET/HEAD are reads;
    // anything else (including lowercase `get`) maps to the write verb.
    #[test]
    fn session_action_is_case_sensitive() {
        assert_eq!(session_action("get").0, "agent.write");
        assert_eq!(session_action("head").0, "agent.write");
        assert_eq!(session_action("HEAD").0, "agent.read");
    }
}
