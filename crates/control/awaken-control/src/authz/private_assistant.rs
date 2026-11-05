// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Private HTTP admission combines current native Account,
// @formal reason: Workspace-purpose and PDP authority before selecting the Session view.
//! Explicit private Session read ingress over the original Managed router.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_authz_enforce::RequestTenancy;
use awaken_iam_contract::{AccountId, AuthorizationDecision, PrincipalRef};
use awaken_iam_host::AuthReject;
use awaken_protocol_managed::AdmittedPrivateSessionRequest;
use awaken_session_contract::SessionPrivateBinding;
use awaken_tenancy::{WorkspaceScope, workspace_purpose::WorkspacePurposeUnavailable};
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use tower::ServiceExt as _;

use super::{
    ActionNamespace, HttpOrdinaryWorkspaceAdmission, RemoteManagementAuthz, RouteAuthz, action_for,
    authorization_bearer_token, forbidden, iam_unavailable, off_event_loop, target_scope,
    unauthenticated,
};

/// Deployment-selected observer of the current private application coordinates.
/// Its response supplies provenance, not Account authentication or PDP authority.
pub struct HttpPrivateWorkspaceContext {
    transport: HttpOrdinaryWorkspaceAdmission,
}

impl HttpPrivateWorkspaceContext {
    pub fn new(endpoint: &str, ordinary: &HttpOrdinaryWorkspaceAdmission) -> Result<Self, String> {
        Ok(Self {
            transport: ordinary.sibling_endpoint(endpoint)?,
        })
    }

    async fn observe(
        &self,
        workspace: &str,
        account: &AccountId,
    ) -> Result<Option<SessionPrivateBinding>, WorkspacePurposeUnavailable> {
        let body = serde_json::json!({"workspace_id": workspace,"account_id": account}).to_string();
        let reply = self.transport.observe_json(&body).await?;
        let binding: SessionPrivateBinding = match reply.status() {
            404 if reply.body().is_empty() => return Ok(None),
            200 if reply.body().len() <= 4096 => {
                serde_json::from_str(reply.body()).map_err(|_| WorkspacePurposeUnavailable)?
            }
            _ => return Err(WorkspacePurposeUnavailable),
        };
        if binding.workspace_id() != workspace || binding.account_id() != account.0 {
            return Err(WorkspacePurposeUnavailable);
        }
        Ok(Some(binding))
    }
}

#[derive(Clone)]
struct Ingress {
    sessions: Router,
    iam: Arc<RemoteManagementAuthz>,
    context: Arc<HttpPrivateWorkspaceContext>,
}

/// Mount the finite read surface. The caller supplies the same Managed Session
/// router and state used by ordinary routes. Addressed Workspace selection is
/// required; the flat platform default cannot select a private application.
pub fn private_session_read_router(
    sessions: Router,
    iam: Arc<RemoteManagementAuthz>,
    context: Arc<HttpPrivateWorkspaceContext>,
) -> Router {
    Router::new()
        .route("/v1/private-assistant/{*operation}", get(dispatch))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
        .with_state(Ingress {
            sessions,
            iam,
            context,
        })
}

fn canonical_read(method: &Method, path: &str) -> Option<String> {
    if method != Method::GET {
        return None;
    }
    let operation = path.strip_prefix("/v1/private-assistant/")?;
    let segments = operation.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    match segments.as_slice() {
        ["sessions"]
        | ["sessions", _]
        | ["sessions", _, "events" | "threads" | "resources"]
        | ["sessions", _, "threads" | "resources", _]
        | ["sessions", _, "threads", _, "events"] => Some(format!("/v1/{operation}")),
        _ => None,
    }
}

async fn dispatch(State(ingress): State<Ingress>, request: Request) -> Response {
    let Some(canonical) = canonical_read(request.method(), request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(workspace) = request
        .extensions()
        .get::<RequestTenancy>()
        .map(|tenancy| tenancy.workspace_id.clone())
        .filter(|workspace| !workspace.is_empty())
    else {
        return forbidden("private Sessions require an addressed Workspace");
    };
    let Some(token) = authorization_bearer_token(request.headers()) else {
        return unauthenticated("private Sessions require a current Account");
    };
    let token = RedactedString::new(token);
    let iam = ingress.iam.clone();
    let account = match off_event_loop::run(move || {
        iam.current_direct_account(token.expose_secret())
    })
    .await
    {
        Ok(Ok(account)) => account,
        Ok(Err(AuthReject::Invalid | AuthReject::Expired | AuthReject::Revoked)) => {
            return unauthenticated("private Sessions require a current Account");
        }
        Ok(Err(AuthReject::Unavailable)) | Err(_) => return iam_unavailable(),
    };
    let binding = match ingress.context.observe(&workspace, &account).await {
        Ok(Some(binding)) => binding,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return iam_unavailable(),
    };
    let (action, scope) = match action_for(request.method(), &canonical) {
        Some(RouteAuthz::HostedRuntime { action, scope, .. }) => (action, scope),
        _ => return forbidden("no native action is mapped for this private operation"),
    };
    let Some(scope) = target_scope(scope, &workspace, &canonical) else {
        return forbidden("no private authorization target");
    };
    let principal = PrincipalRef::Account {
        account_id: account,
    };
    let decision_principal = principal.clone();
    let iam = ingress.iam.clone();
    match off_event_loop::run(move || {
        iam.authorize_action(
            decision_principal,
            action,
            scope,
            ActionNamespace::HostedRuntime,
        )
    })
    .await
    {
        Ok(Ok(AuthorizationDecision::Allow)) => {}
        Ok(Ok(AuthorizationDecision::Deny | AuthorizationDecision::RequireApproval)) => {
            return forbidden("private operation is not authorized");
        }
        Ok(Err(_)) | Err(_) => return iam_unavailable(),
    }
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let Ok(uri) = format!("{canonical}{query}").parse::<Uri>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (mut parts, body) = request.into_parts();
    parts.uri = uri;
    // The alias catch-all captured a different set of path parameters. Keep
    // only the freshly admitted authority and let the native router re-extract
    // the canonical Session id. The original bearer never reaches handlers.
    parts.extensions = axum::http::Extensions::new();
    parts.extensions.insert(WorkspaceScope(workspace.clone()));
    parts.extensions.insert(RequestTenancy {
        workspace_id: workspace,
    });
    parts.extensions.insert(principal);
    parts
        .extensions
        .insert(AdmittedPrivateSessionRequest::from_admitted(binding));
    parts.headers.remove("authorization");
    parts.headers.remove("x-api-key");
    ingress
        .sessions
        .oneshot(Request::from_parts(parts, body))
        .await
        .unwrap_or_else(|error| match error {})
}

#[cfg(test)]
#[path = "private_assistant_tests.rs"]
mod tests;
