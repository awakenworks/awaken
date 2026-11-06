// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Private HTTP admission combines current native Account,
// @formal reason: Workspace-purpose and PDP authority before selecting the Session view.
//! Explicit private Session and File operations over the original Managed router.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_authz_enforce::RequestTenancy;
use awaken_iam_contract::{AccountId, AuthorizationDecision, PrincipalRef};
use awaken_iam_host::AuthReject;
use awaken_protocol_managed::AdmittedPrivateSessionRequest;
use awaken_session_contract::SessionPrivateBinding;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use awaken_tenancy::workspace_purpose::WorkspacePurposeUnavailable;
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use tower::ServiceExt as _;

use super::{
    ActionNamespace, HttpOrdinaryWorkspaceAdmission, RemoteManagementAuthz, RouteAuthz, ScopeClass,
    action_for, authorization_bearer_token, forbidden, iam_unavailable, off_event_loop,
    target_scope, unauthenticated,
};

mod writes;
use writes::{Profiles, Write};

/// Deployment-selected observer of the current private application coordinates.
/// Its response supplies provenance, not Account authentication or PDP authority.
pub struct HttpPrivateWorkspaceContext {
    transport: HttpOrdinaryWorkspaceAdmission,
    writes: Option<Profiles>,
}

impl HttpPrivateWorkspaceContext {
    pub fn new(endpoint: &str, ordinary: &HttpOrdinaryWorkspaceAdmission) -> Result<Self, String> {
        Ok(Self {
            transport: ordinary.sibling_endpoint(endpoint)?,
            writes: None,
        })
    }

    /// Install exact deployment-owned Agent and Environment selections.
    pub fn with_write_profiles(mut self, bytes: &[u8]) -> Result<Self, String> {
        self.writes = Some(Profiles::parse(bytes)?);
        Ok(self)
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
    managed: Router,
    iam: Arc<RemoteManagementAuthz>,
    context: Arc<HttpPrivateWorkspaceContext>,
}

/// Mount the finite private surface. The caller supplies the same Managed
/// router and state used by ordinary routes. Addressed Workspace selection is
/// required; the flat platform default cannot select a private application.
pub fn private_assistant_router(
    managed: Router,
    iam: Arc<RemoteManagementAuthz>,
    context: Arc<HttpPrivateWorkspaceContext>,
) -> Router {
    Router::new()
        .route(
            "/v1/private-assistant/{*operation}",
            get(dispatch).post(dispatch),
        )
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
            managed,
            iam,
            context,
        })
}

fn canonical_read(method: &Method, path: &str) -> Option<(String, ActionNamespace)> {
    if method != Method::GET {
        return None;
    }
    let operation = path.strip_prefix("/v1/private-assistant/")?;
    let segments = operation.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let namespace = match segments.as_slice() {
        ["sessions"]
        | ["sessions", _]
        | ["sessions", _, "events" | "threads" | "resources"]
        | ["sessions", _, "threads" | "resources", _]
        | ["sessions", _, "events", "stream"]
        | ["sessions", _, "threads", _, "events" | "stream"] => ActionNamespace::HostedRuntime,
        ["files", _] | ["files", _, "content"] => ActionNamespace::Workspace,
        _ => return None,
    };
    Some((format!("/v1/{operation}"), namespace))
}

async fn dispatch(State(ingress): State<Ingress>, request: Request) -> Response {
    let (canonical, namespace, write) = if request.method() == Method::POST {
        let Some((canonical, write)) = Write::classify(request.uri().path()) else {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        };
        if !matches!(write, Write::Stop) && ingress.context.writes.is_none() {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        if request.uri().query().is_some()
            || !matches!(
                awaken_protocol_managed::parse_idempotency_key_header(request.headers()),
                Ok(Some(_))
            )
        {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let namespace = if matches!(write, Write::Upload) {
            ActionNamespace::Workspace
        } else {
            ActionNamespace::HostedRuntime
        };
        (canonical, namespace, Some(write))
    } else {
        let Some((canonical, namespace)) = canonical_read(request.method(), request.uri().path())
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (canonical, namespace, None)
    };
    let Some(workspace) = request
        .extensions()
        .get::<RequestTenancy>()
        .map(|tenancy| tenancy.workspace_id.clone())
        .filter(|workspace| !workspace.is_empty())
    else {
        return forbidden("private content requires an addressed Workspace");
    };
    let Some(token) = authorization_bearer_token(request.headers()) else {
        return unauthenticated("private content requires a current Account");
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
            return unauthenticated("private content requires a current Account");
        }
        Ok(Err(AuthReject::Unavailable)) | Err(_) => return iam_unavailable(),
    };
    let binding = match ingress.context.observe(&workspace, &account).await {
        Ok(Some(binding)) => binding,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return iam_unavailable(),
    };
    let profile = ingress
        .context
        .writes
        .as_ref()
        .and_then(|profiles| profiles.for_binding(&binding));
    if write
        .as_ref()
        .is_some_and(|write| !matches!(write, Write::Stop))
        && profile.is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (action, scope) = if matches!(write, Some(Write::Stop)) {
        ("run.cancel", ScopeClass::Workspace)
    } else {
        match (namespace, action_for(request.method(), &canonical)) {
            (
                ActionNamespace::HostedRuntime,
                Some(RouteAuthz::HostedRuntime { action, scope, .. }),
            )
            | (ActionNamespace::Workspace, Some(RouteAuthz::Resource { action, scope })) => {
                (action, scope)
            }
            _ => return forbidden("no native action is mapped for this private operation"),
        }
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
        iam.authorize_action(decision_principal, action, scope, namespace)
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
    let body = if let Some(write) = write {
        match write.body(&mut parts.headers, body, profile).await {
            Ok(body) => body,
            Err(status) => return status.into_response(),
        }
    } else {
        body
    };
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
        .managed
        .oneshot(Request::from_parts(parts, body))
        .await
        .unwrap_or_else(|error| match error {})
}

#[cfg(test)]
#[path = "private_assistant_tests.rs"]
mod tests;
