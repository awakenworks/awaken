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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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

/// The deliberately small authority Awaken accepts from a customer application.
///
/// `application_scope` and `actor_key` are opaque strings chosen by the
/// embedding application. Awaken does not interpret them as users, roles, or
/// projects. Runtime identity comes only from explicit Managed Session bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationGrant {
    pub authority_id: String,
    pub application_scope: String,
    pub actor_key: Option<String>,
    pub protocols: HashSet<String>,
    pub operations: HashSet<String>,
    pub thread_bindings: HashMap<String, ApplicationThreadBinding>,
}

/// One application-visible thread alias resolved to an existing Managed Session.
///
/// The Managed Session remains the resource authority. This value is only the
/// short-lived authorization projection carried by an application grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationThreadBinding {
    pub managed_session_id: String,
    pub agent_id: String,
}

impl ApplicationGrant {
    /// Resolve only an alias explicitly authorized by this short-lived grant.
    #[must_use]
    pub fn binding(&self, external_thread_id: &str) -> Option<&ApplicationThreadBinding> {
        self.thread_bindings.get(external_thread_id)
    }
}

/// An application credential after authentication. It is intentionally
/// separate from the service credential directory used by management APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationIdentity {
    pub workspace_id: String,
    pub grant: ApplicationGrant,
}

/// Process-local short-lived application credentials.
///
/// Restarting the binary invalidates these credentials, which is a safe default
/// for phase one. Continuing a conversation after rotation requires the issuer
/// to project the same explicit Managed Session binding into the replacement.
pub struct ApplicationAccessStore {
    engine: EnforceEngine,
    grants: Mutex<HashMap<String, ApplicationGrant>>,
}

impl Default for ApplicationAccessStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationAccessStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: EnforceEngine::seeded(),
            grants: Mutex::new(HashMap::new()),
        }
    }

    /// Mint one short-lived credential and attach its narrow application grant.
    pub fn mint(
        &self,
        token_id: String,
        workspace_id: String,
        expires_at: Option<String>,
        grant: ApplicationGrant,
    ) -> Result<String, IamError> {
        let service_id = format!("application:{token_id}");
        let secret = self.engine.mint(TokenSpec {
            token_id,
            service_id: service_id.clone(),
            workspace_id,
            role: "admin".to_string(),
            expires_at,
        })?;
        self.grants
            .lock()
            .expect("application grant store poisoned")
            .insert(service_id, grant);
        Ok(secret)
    }

    /// Authenticate only credentials minted by this application store.
    pub fn authenticate(&self, presented: &str) -> Option<ApplicationIdentity> {
        let (principal, workspace) = self.engine.authenticate(presented).ok()?;
        let PrincipalRef::Service { service_id } = principal else {
            return None;
        };
        let grant = self
            .grants
            .lock()
            .expect("application grant store poisoned")
            .get(&service_id)
            .cloned()?;
        Some(ApplicationIdentity {
            workspace_id: workspace.0,
            grant,
        })
    }

    pub fn revoke(&self, token_id: &str) -> Result<(), IamError> {
        self.engine.revoke(token_id)?;
        self.grants
            .lock()
            .expect("application grant store poisoned")
            .remove(&format!("application:{token_id}"));
        Ok(())
    }
}

const MAX_APPLICATION_BODY: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct ApplicationRejection {
    status: StatusCode,
    detail: &'static str,
}

impl ApplicationRejection {
    const fn new(status: StatusCode, detail: &'static str) -> Self {
        Self { status, detail }
    }

    fn into_response(self) -> Response {
        reject(self.status, self.detail)
    }
}

/// Blanket PEP for browser/application protocol routes.
///
/// It accepts only an application credential, classifies the exact public route,
/// checks protocol/action authority, then resolves one explicitly bound external
/// thread id to its existing Managed Session before a protocol adapter sees the
/// request. Unknown routes and unbound ids fail closed; this guard never creates
/// or derives a second runtime identity.
pub async fn application_guard(
    State(store): State<Arc<ApplicationAccessStore>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(presented) = presented_bearer(request.headers()) else {
        return reject(StatusCode::UNAUTHORIZED, "missing application access token");
    };
    let Some(identity) = store.authenticate(&presented) else {
        return reject(StatusCode::UNAUTHORIZED, "invalid application access token");
    };
    if let Some(tenancy) = request.extensions().get::<RequestTenancy>()
        && tenancy.workspace_id != identity.workspace_id
    {
        return reject(
            StatusCode::FORBIDDEN,
            "application token does not allow this workspace",
        );
    }
    // The application credential is a narrow Workspace authority, not merely
    // an opaque thread lookup key. Publish the exact authenticated scope for
    // downstream protocol projections after fencing any path selection.
    request.extensions_mut().insert(RequestTenancy {
        workspace_id: identity.workspace_id.clone(),
    });
    request
        .extensions_mut()
        .insert(awaken_tenancy::WorkspaceScope(
            identity.workspace_id.clone(),
        ));
    let path = request.uri().path().to_string();
    let Some(route) = classify_application_route(request.method(), &path) else {
        return reject(
            StatusCode::FORBIDDEN,
            "application token does not allow this route",
        );
    };
    if !identity.grant.protocols.contains(route.protocol) {
        return reject(
            StatusCode::FORBIDDEN,
            "application token does not allow this protocol",
        );
    }
    if !identity.grant.operations.contains(route.operation) {
        return reject(
            StatusCode::FORBIDDEN,
            "application token does not allow this operation",
        );
    }
    match scope_application_request(request, &identity, route).await {
        Ok(request) => next.run(request).await,
        Err(rejection) => rejection.into_response(),
    }
}

#[derive(Debug, Clone, Copy)]
struct ApplicationRoute<'a> {
    protocol: &'static str,
    operation: &'static str,
    path_thread: Option<&'a str>,
    path_agent: Option<&'a str>,
}

/// The single default-deny route-to-authority table for application protocols.
/// Adding a GET route does not silently grant it history access merely because of
/// its HTTP method.
fn classify_application_route<'a>(method: &Method, path: &'a str) -> Option<ApplicationRoute<'a>> {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let route = match (method, segments.as_slice()) {
        (&Method::POST, ["v1", "ai-sdk", "chat"]) => ApplicationRoute::run("ai-sdk", None, None),
        (&Method::POST, ["v1", "ai-sdk", "threads", thread, "runs"]) => {
            ApplicationRoute::run("ai-sdk", Some(thread), None)
        }
        (&Method::POST, ["v1", "ai-sdk", "agents", agent, "runs"]) => {
            ApplicationRoute::run("ai-sdk", None, Some(agent))
        }
        (&Method::GET | &Method::HEAD, ["v1", "ai-sdk", "threads", thread, "messages"]) => {
            ApplicationRoute::messages("ai-sdk", thread)
        }
        (&Method::POST, ["v1", "ag-ui"]) => ApplicationRoute::run("ag-ui", None, None),
        (&Method::POST, ["v1", "ag-ui", "agents", agent]) => {
            ApplicationRoute::run("ag-ui", None, Some(agent))
        }
        (&Method::GET | &Method::HEAD, ["v1", "ag-ui", "threads", thread, "messages"]) => {
            ApplicationRoute::messages("ag-ui", thread)
        }
        _ => return None,
    };
    Some(route)
}

impl<'a> ApplicationRoute<'a> {
    const fn run(
        protocol: &'static str,
        path_thread: Option<&'a str>,
        path_agent: Option<&'a str>,
    ) -> Self {
        Self {
            protocol,
            operation: "thread.run",
            path_thread,
            path_agent,
        }
    }

    const fn messages(protocol: &'static str, path_thread: &'a str) -> Self {
        Self {
            protocol,
            operation: "thread.messages.read",
            path_thread: Some(path_thread),
            path_agent: None,
        }
    }
}

async fn scope_application_request<'a>(
    request: Request,
    identity: &ApplicationIdentity,
    route: ApplicationRoute<'a>,
) -> Result<Request, ApplicationRejection> {
    if request.method() == Method::POST {
        let (mut parts, body) = request.into_parts();
        let bytes = to_bytes(body, MAX_APPLICATION_BODY).await.map_err(|_| {
            ApplicationRejection::new(StatusCode::BAD_REQUEST, "invalid application request body")
        })?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
            ApplicationRejection::new(StatusCode::BAD_REQUEST, "application request must be JSON")
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            ApplicationRejection::new(
                StatusCode::BAD_REQUEST,
                "application request must be an object",
            )
        })?;
        let thread_key = if object.contains_key("threadId") {
            Some("threadId")
        } else if object.contains_key("thread_id") {
            Some("thread_id")
        } else {
            None
        };
        let body_thread = match thread_key {
            Some(key) => Some(
                object
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        ApplicationRejection::new(
                            StatusCode::BAD_REQUEST,
                            "thread id must be a string",
                        )
                    })?,
            ),
            None => None,
        };
        if let (Some(path_thread), Some(body_thread)) = (route.path_thread, body_thread)
            && path_thread != body_thread
        {
            return Err(ApplicationRejection::new(
                StatusCode::BAD_REQUEST,
                "path and body thread ids must match",
            ));
        }
        let external = route.path_thread.or(body_thread).ok_or_else(|| {
            ApplicationRejection::new(
                StatusCode::BAD_REQUEST,
                "application protocol requests require a thread id",
            )
        })?;
        let binding = resolve_binding(identity, external)?;
        authorize_bound_agent(route.path_agent, object, binding)?;

        if let Some(key) = thread_key {
            object.insert(
                key.to_string(),
                serde_json::Value::String(binding.managed_session_id.clone()),
            );
        }
        if route.protocol == "ai-sdk" {
            object.insert(
                "agentId".to_string(),
                serde_json::Value::String(binding.agent_id.clone()),
            );
            object.remove("agent_id");
        }
        parts.extensions.insert(awaken_tenancy::ResolvedResourceId(
            binding.managed_session_id.clone(),
        ));
        parts
            .extensions
            .insert(awaken_tenancy::ResolvedAgentId(binding.agent_id.clone()));
        let encoded = serde_json::to_vec(&value).map_err(|_| {
            ApplicationRejection::new(StatusCode::BAD_REQUEST, "invalid application request body")
        })?;
        parts.headers.remove(header::CONTENT_LENGTH);
        return Ok(Request::from_parts(parts, Body::from(encoded)));
    }

    let external = route.path_thread.ok_or_else(|| {
        ApplicationRejection::new(
            StatusCode::BAD_REQUEST,
            "application protocol requests require a thread id",
        )
    })?;
    let binding = resolve_binding(identity, external)?;
    let (mut parts, body) = request.into_parts();
    parts.extensions.insert(awaken_tenancy::ResolvedResourceId(
        binding.managed_session_id.clone(),
    ));
    parts
        .extensions
        .insert(awaken_tenancy::ResolvedAgentId(binding.agent_id.clone()));
    let request = Request::from_parts(parts, body);
    Ok(request)
}

fn resolve_binding<'a>(
    identity: &'a ApplicationIdentity,
    external_thread_id: &str,
) -> Result<&'a ApplicationThreadBinding, ApplicationRejection> {
    identity.grant.binding(external_thread_id).ok_or_else(|| {
        ApplicationRejection::new(
            StatusCode::FORBIDDEN,
            "application token does not allow this thread",
        )
    })
}

fn authorize_bound_agent(
    path_agent: Option<&str>,
    object: &serde_json::Map<String, serde_json::Value>,
    binding: &ApplicationThreadBinding,
) -> Result<(), ApplicationRejection> {
    let body_agent_value = object.get("agentId").or_else(|| object.get("agent_id"));
    let body_agent = match body_agent_value {
        Some(serde_json::Value::String(agent)) => Some(agent.as_str()),
        Some(_) => {
            return Err(ApplicationRejection::new(
                StatusCode::BAD_REQUEST,
                "agent id must be a string",
            ));
        }
        None => None,
    };
    if path_agent
        .into_iter()
        .chain(body_agent)
        .any(|agent| agent != binding.agent_id)
    {
        return Err(ApplicationRejection::new(
            StatusCode::FORBIDDEN,
            "application token does not allow this agent",
        ));
    }
    Ok(())
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

    /// Revoke a token by id in the process-local directory.
    pub fn revoke(&self, token_id: &str) -> Result<(), IamError> {
        self.state
            .lock()
            .expect("enforce engine poisoned")
            .directory
            .revoke(
                &awaken_iam_contract::ApiTokenId(token_id.to_string()),
                Timestamp(now_rfc3339()),
            )
    }

    /// Authorize `principal` performing `action` at `scope`. Fail-closed: an
    /// unmatched request is [`SessionDecision::Deny`]. The shared engine's
    /// three-way decision is collapsed at this single boundary into the session
    /// surface's two-way outcome — see [`SessionDecision`].
    pub fn authorize(
        &self,
        principal: PrincipalRef,
        action: &ActionKey,
        scope: ScopeRef,
    ) -> SessionDecision {
        let request = AuthorizationRequest::direct(principal, action.clone(), scope);
        let decision = self
            .state
            .lock()
            .expect("enforce engine poisoned")
            .policy
            .evaluate(&request)
            .decision;
        collapse_session_decision(decision)
    }
}

/// Collapse the shared engine's three-way [`AuthorizationDecision`] into the session
/// surface's two-way [`SessionDecision`], **fail-closed**: only an explicit `Allow`
/// allows; both a bare `Deny` and an `RequireApproval` degrade to `Deny`. Approval is a
/// management-plane concept the seeded engine never installs at the session axis, so the
/// `RequireApproval` arm is defensive — but it is pinned here (rather than inlined) so a
/// future refactor cannot silently map it to `Allow` and open the session surface. See
/// [`SessionDecision`].
#[must_use]
fn collapse_session_decision(decision: AuthorizationDecision) -> SessionDecision {
    match decision {
        AuthorizationDecision::Allow => SessionDecision::Allow,
        AuthorizationDecision::Deny | AuthorizationDecision::RequireApproval => {
            SessionDecision::Deny
        }
    }
}

/// The session surface's authorization outcome. It has no `RequireApproval`
/// variant: this in-memory seeded engine installs only Allow grants, so approval —
/// a management-plane concept — cannot arise here. Mapping the shared engine's
/// decision at this one boundary makes the illegal-at-this-layer state unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDecision {
    Allow,
    Deny,
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
///
/// INVARIANT (fail-closed by construction): this MUST be mounted as a **blanket**
/// layer over the whole router, never as a per-route `.route_layer`. Because the
/// action is a total function of the HTTP method ([`session_action`]) and the path
/// is never consulted, there is no "unmapped route" that could default-allow — a
/// newly mounted route inherits the gate automatically. A refactor to per-route
/// layering would reintroduce exactly that fail-open, so do not do it.
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
        SessionDecision::Allow => {
            // Publish the edge-resolved owning workspace so a downstream projection
            // (webhooks/usage) can stamp it — the aspect resolves tenancy, the core
            // never stores it. Idempotent: a project-less deployment has no prior
            // `RequestTenancy`, a future one would already carry the same value.
            request
                .extensions_mut()
                .insert(RequestTenancy { workspace_id });
            next.run(request).await
        }
        SessionDecision::Deny => reject(StatusCode::FORBIDDEN, "not authorized for this scope"),
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

    // M6/T44: the three-way→two-way collapse at the session boundary is fail-closed.
    // `Allow` is the only decision that allows; both a bare `Deny` and — critically — a
    // `RequireApproval` (an approval requirement the seeded engine never installs, hence
    // otherwise unreachable through the public seam) degrade to `Deny`. Pinning all three
    // rows here guarantees a refactor cannot fold `RequireApproval` into `Allow` and open
    // the session surface to an un-approved request.
    #[test]
    fn require_approval_collapses_to_deny_fail_closed() {
        assert_eq!(
            collapse_session_decision(AuthorizationDecision::Allow),
            SessionDecision::Allow
        );
        assert_eq!(
            collapse_session_decision(AuthorizationDecision::Deny),
            SessionDecision::Deny
        );
        assert_eq!(
            collapse_session_decision(AuthorizationDecision::RequireApproval),
            SessionDecision::Deny,
            "an approval requirement must fail closed at the session axis, never allow"
        );
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
        assert_eq!(write, SessionDecision::Allow);
        let read = engine.authorize(principal, &session_action("GET"), request_scope(WS));
        assert_eq!(read, SessionDecision::Allow);
    }

    #[test]
    fn another_workspace_is_denied_even_for_admin() {
        let (engine, _secret, principal) = engine_with_admin();
        let cross = engine.authorize(
            principal,
            &session_action("POST"),
            request_scope("wrkspc_other"),
        );
        assert_eq!(cross, SessionDecision::Deny, "the scope fence holds");
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

    // --- CEG completeness: authorize seam (grant × action × scope) -----------

    // A non-admin role that carries grants — just none under `agent.*` — is
    // denied the managed-agent surface at its OWN workspace, for reads and
    // writes alike. This is the pure-authorize analog of the guard's policy gate:
    // a scope-clean request still fails default-deny (grant for the wrong
    // action). `developer` holds `apikey.*` but never the agent verbs.
    #[test]
    fn authorize_denies_a_non_admin_at_its_own_workspace() {
        let (engine, secret) = engine_with_role("developer");
        let (principal, _ws) = engine.authenticate(&secret).expect("authenticate");
        assert_eq!(
            engine.authorize(
                principal.clone(),
                &session_action("POST"),
                request_scope(WS)
            ),
            SessionDecision::Deny,
            "a role without agent.* is denied writes at its own workspace",
        );
        assert_eq!(
            engine.authorize(principal, &session_action("GET"), request_scope(WS)),
            SessionDecision::Deny,
            "…and reads too — default-deny is not method-specific",
        );
    }

    // Grant-for-the-wrong-action: `billing` holds `billing.*` and nothing else,
    // so an `agent.write` request finds no matching grant → default deny. Proves
    // a non-empty grant set in an unrelated namespace never leaks authority.
    #[test]
    fn authorize_denies_a_wrong_namespace_role() {
        let (engine, secret) = engine_with_role("billing");
        let (principal, _ws) = engine.authenticate(&secret).expect("authenticate");
        assert_eq!(
            engine.authorize(principal, &session_action("POST"), request_scope(WS)),
            SessionDecision::Deny,
        );
    }

    // --- CEG completeness: guard gates (fence vs policy, read path, headers) --

    // The scope fence (E3) and the policy engine (E4) are two distinct 403 gates.
    // A non-admin whose named workspace MATCHES its token passes the fence
    // (`resolve_scope` → Ok) yet is still denied by the policy engine. Without
    // this row the developer-deny case could be a fence artifact rather than a
    // policy decision.
    #[tokio::test]
    async fn guard_forbids_a_non_admin_even_at_its_own_named_workspace() {
        let (engine, secret) = engine_with_role("developer");
        let tenancy = RequestTenancy {
            workspace_id: WS.into(),
        };
        assert_eq!(
            call(engine, Method::POST, Some(&secret), Some(tenancy)).await,
            StatusCode::FORBIDDEN,
        );
    }

    // Grant-for-the-wrong-action through the full guard: a `billing` token
    // authenticates and is scope-clean, but its grants are another namespace → 403.
    #[tokio::test]
    async fn guard_forbids_a_wrong_namespace_grant() {
        let (engine, secret) = engine_with_role("billing");
        assert_eq!(
            call(engine, Method::POST, Some(&secret), None).await,
            StatusCode::FORBIDDEN,
        );
    }

    // The read action (GET → `agent.read`) is exercised end-to-end through the
    // guard, not just at the `session_action` unit. Admin holds `agent.*`, so the
    // read is allowed.
    #[tokio::test]
    async fn guard_allows_an_admin_read() {
        let (engine, secret) = admin_engine();
        assert_eq!(
            call(engine, Method::GET, Some(&secret), None).await,
            StatusCode::OK,
        );
    }

    // The SDK credential header (`x-api-key`) authenticates through the full
    // guard, not only through the `presented_bearer` unit — a valid admin key in
    // `x-api-key` reaches the handler.
    #[tokio::test]
    async fn guard_authenticates_via_the_x_api_key_header() {
        let (engine, secret) = admin_engine();
        let app = Router::new()
            .fallback(any(|| async { "ok" }))
            .layer(from_fn_with_state(engine, guard));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/sessions")
            .header("x-api-key", secret)
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            app.oneshot(request).await.expect("router call").status(),
            StatusCode::OK,
        );
    }

    // No route is exempt from the guard. The action is derived from the HTTP
    // method, never the path, so an arbitrary/unknown URI is gated identically:
    // a non-admin is denied and an admin is allowed on the very same unmapped
    // path. This closes the "unknown route → default-allow" fail-open: there is
    // no route allowlist that a new/unmapped path could slip through.
    #[tokio::test]
    async fn guard_gates_an_unmapped_route_the_same_as_any_other() {
        let unknown = "/totally/unknown/route";

        let (deny_engine, dev_secret) = engine_with_role("developer");
        let deny_app = Router::new()
            .fallback(any(|| async { "ok" }))
            .layer(from_fn_with_state(deny_engine, guard));
        let deny_req = Request::builder()
            .method(Method::POST)
            .uri(unknown)
            .header("authorization", format!("Bearer {dev_secret}"))
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            deny_app.oneshot(deny_req).await.expect("call").status(),
            StatusCode::FORBIDDEN,
            "an unmapped route is still policy-gated for a non-admin",
        );

        let (allow_engine, admin_secret) = admin_engine();
        let allow_app = Router::new()
            .fallback(any(|| async { "ok" }))
            .layer(from_fn_with_state(allow_engine, guard));
        let allow_req = Request::builder()
            .method(Method::POST)
            .uri(unknown)
            .header("authorization", format!("Bearer {admin_secret}"))
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            allow_app.oneshot(allow_req).await.expect("call").status(),
            StatusCode::OK,
            "the same unmapped route is reachable for an authorized admin",
        );
    }
}
