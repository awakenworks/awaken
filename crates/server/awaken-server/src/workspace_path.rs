//! Management-plane workspace path addressing (ADR-0048 D3 / ADR-0051).
//!
//! The management plane addresses a workspace-scoped resource by putting the
//! workspace in the path: `/v1/workspaces/{ws}/config/agents/{id}`. Rather than
//! duplicate every `/v1/config/*`, `/v1/agents/*`, `/v1/vaults/*` route under a
//! `/v1/workspaces/{ws}` prefix, a single catch-all route captures
//! `/v1/workspaces/{ws}/{*rest}`, rewrites the request to its flat `/v1/{rest}`
//! form, stamps the resolved `{ws}` as the edge scope ([`WorkspaceScope`] +
//! [`RequestTenancy`]) — exactly what a workspace API key would have resolved to —
//! and forwards it into the flat management router. The existing handlers then run
//! scoped and the per-resource ownership guards (ADR-0051) fence cross-tenant
//! access.
//!
//! This is a *route* (not a `Router::layer`) because an unmatched
//! `/v1/workspaces/{ws}/…` path must be re-routed after rewriting, which a layer —
//! running after route selection — cannot do. Flat requests fall through to the
//! same router unchanged (`fallback_service`), so this is purely additive; the
//! data plane (flat `/v1/sessions`, workspace-from-key) is never prefixed.

use awaken_authz_enforce::RequestTenancy;
use awaken_protocol_managed::WorkspaceScope;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::Uri;
use axum::response::Response;
use axum::routing::any;
use tower::ServiceExt;

/// Wrap a flat management `Router` with workspace path addressing: a
/// `/v1/workspaces/{ws}/{rest}` request is rewritten to `/v1/{rest}`, scoped to
/// `{ws}`, and forwarded into `flat`; every other path falls through to `flat`
/// unchanged.
pub fn with_workspace_path_addressing(flat: Router) -> Router {
    Router::new()
        .route("/v1/workspaces/{ws}/{*rest}", any(dispatch))
        .with_state(flat.clone())
        .fallback_service(flat)
}

/// Rewrite `/v1/workspaces/{ws}/{rest}` → `/v1/{rest}`, stamp the scope, and
/// forward into the flat router.
async fn dispatch(
    State(flat): State<Router>,
    Path((ws, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let (mut parts, body) = request.into_parts();
    // A rewritten path is always a valid relative URI; if it somehow is not, leave
    // the path as-is (the flat router will 404) rather than panic.
    if let Ok(uri) = format!("/v1/{rest}{query}").parse::<Uri>() {
        parts.uri = uri;
    }
    // Rebuild with FRESH extensions: the outer catch-all route stored its own
    // matched path params (`{ws}`/`{*rest}`) in the request, which would collide with
    // the flat router's `{id}` extraction and 500 it. Drop them; carry only the
    // resolved scope so the handlers + ownership guards see the tenancy.
    let mut extensions = axum::http::Extensions::new();
    extensions.insert(WorkspaceScope(ws.clone()));
    extensions.insert(RequestTenancy { workspace_id: ws });
    parts.extensions = extensions;
    let request = Request::from_parts(parts, body);
    // `Router`'s service error is `Infallible`, so this never fails.
    flat.oneshot(request)
        .await
        .unwrap_or_else(|err| match err {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use http_body_util::BodyExt;

    /// A flat router that echoes the matched path and the stamped scope, so a test
    /// can assert the rewrite target and the resolved workspace.
    fn echo_router() -> Router {
        async fn echo(uri: Uri, scope: Option<axum::Extension<WorkspaceScope>>) -> String {
            let ws = scope.map_or_else(|| "-".to_string(), |w| w.0.0.clone());
            format!("{}|{ws}", uri.path())
        }
        Router::new()
            .route("/v1/agents", get(echo))
            .route("/v1/config/agents/{id}", get(echo))
    }

    async fn get_path(app: &Router, uri: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn rewrites_and_scopes_a_workspace_path() {
        let app = with_workspace_path_addressing(echo_router());
        // `/v1/workspaces/ws_a/agents` → `/v1/agents`, scope ws_a.
        let (status, body) = get_path(&app, "/v1/workspaces/ws_a/agents").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "/v1/agents|ws_a");
        // A deeper resource rewrites its whole tail.
        let (status, body) = get_path(&app, "/v1/workspaces/acme/config/agents/x").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "/v1/config/agents/x|acme");
    }

    #[tokio::test]
    async fn a_flat_path_falls_through_unchanged_and_unscoped() {
        let app = with_workspace_path_addressing(echo_router());
        let (status, body) = get_path(&app, "/v1/agents").await;
        assert_eq!(status, StatusCode::OK);
        // No workspace stamped for a flat request.
        assert_eq!(body, "/v1/agents|-");
    }
}
