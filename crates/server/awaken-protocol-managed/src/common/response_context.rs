//! Response metadata consumed directly by the official Anthropic SDKs.

use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{Next, from_fn};
use axum::response::Response;
use uuid::Uuid;

const REQUEST_ID_HEADER: &str = "request-id";
const WORKSPACE_ID_HEADER: &str = "anthropic-workspace-id";

fn generated_request_id() -> HeaderValue {
    let value = format!("req_{}", Uuid::now_v7().simple());
    match HeaderValue::from_str(&value) {
        Ok(value) => value,
        // `value` contains only the fixed ASCII prefix and lower hexadecimal
        // UUID digits. Keep the HTTP boundary total even if a future formatter
        // accidentally violates that invariant; the conformance test rejects
        // this sentinel before it can become an accepted protocol value.
        Err(_) => HeaderValue::from_static("invalid-generated-request-id"),
    }
}

fn workspace_header_value(workspace: &str) -> Option<HeaderValue> {
    if workspace.trim().is_empty() {
        return None;
    }
    HeaderValue::from_str(workspace).ok()
}

/// Project the workspace selected by an authenticated edge into the official
/// Managed Agents response coordinate.
///
/// Authentication owns this value and therefore replaces any untrusted inner
/// projection. Callers that do not own identity should use
/// [`with_managed_response_context`], whose idempotent fallback never replaces
/// an existing response header.
pub fn with_managed_workspace_header(mut response: Response, workspace: &str) -> Response {
    if let Some(value) = workspace_header_value(workspace) {
        response.headers_mut().insert(WORKSPACE_ID_HEADER, value);
    }
    response
}

async fn project_response_context(request: Request, next: Next) -> Response {
    let workspace = request
        .extensions()
        .get::<WorkspaceScope>()
        .and_then(WorkspaceScope::non_empty)
        .and_then(workspace_header_value);
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if !headers.contains_key(REQUEST_ID_HEADER) {
        headers.insert(REQUEST_ID_HEADER, generated_request_id());
    }
    if !headers.contains_key(WORKSPACE_ID_HEADER)
        && let Some(workspace) = workspace
    {
        headers.insert(WORKSPACE_ID_HEADER, workspace);
    }
    response
}

/// Add the response coordinates consumed by `withResponse()`, parsed resource
/// `_request_id`/`_workspace_id` fields, and typed SDK errors.
///
/// The layer is deliberately idempotent: an outer authentication edge may own
/// the Workspace projection and a reverse proxy may already own correlation.
/// Neither authority is overwritten when routers are composed.
pub fn with_managed_response_context(router: Router) -> Router {
    router.layer(from_fn(project_response_context))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    async fn response_with_owned_headers() -> Response {
        let response = (StatusCode::OK, [(REQUEST_ID_HEADER, "req_downstream")]).into_response();
        with_managed_workspace_header(response, "workspace_downstream")
    }

    fn assert_generated_request_id(value: &HeaderValue) {
        let value = value.to_str().unwrap();
        assert_eq!(value.len(), 36);
        assert!(value.starts_with("req_"));
        assert!(value[4..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    // Test design: managed_response_context_is_total_scoped_and_non_overwriting
    //
    // Cause/effect graph:
    // authenticated WorkspaceScope (present / absent)
    //   + downstream response authority (present / absent)
    //   -> official request-id and anthropic-workspace-id response coordinates.
    //
    // Decision table:
    // | scope   | downstream header | effect                         |
    // | present | absent            | fresh id + project scope       |
    // | absent  | absent            | fresh id + no workspace        |
    // | any     | present           | preserve downstream authority  |
    // | any     | caller x-request-id | never trust it as response id |
    //
    // The assertions also prove generated ids are non-empty and distinct, so
    // two requests cannot accidentally share debugging/correlation identity.
    #[tokio::test]
    async fn managed_response_context_is_total_scoped_and_non_overwriting() {
        let app = with_managed_response_context(
            Router::new()
                .route("/generated", get(|| async { StatusCode::NO_CONTENT }))
                .route("/owned", get(response_with_owned_headers)),
        );

        let scoped = || {
            let mut request = Request::builder()
                .uri("/generated")
                .header("x-request-id", "req_caller_controlled")
                .body(Body::empty())
                .expect("valid request");
            request
                .extensions_mut()
                .insert(WorkspaceScope("workspace_authenticated".into()));
            request
        };

        let echoed = app.clone().oneshot(scoped()).await.unwrap();
        assert_generated_request_id(&echoed.headers()[REQUEST_ID_HEADER]);
        assert_ne!(echoed.headers()[REQUEST_ID_HEADER], "req_caller_controlled");
        assert_eq!(
            echoed.headers()[WORKSPACE_ID_HEADER],
            "workspace_authenticated"
        );

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/generated")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/generated")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let first_id = first.headers()[REQUEST_ID_HEADER].to_str().unwrap();
        let second_id = second.headers()[REQUEST_ID_HEADER].to_str().unwrap();
        assert_generated_request_id(&first.headers()[REQUEST_ID_HEADER]);
        assert_generated_request_id(&second.headers()[REQUEST_ID_HEADER]);
        assert_ne!(first_id, second_id);
        assert!(!first.headers().contains_key(WORKSPACE_ID_HEADER));
        assert!(!second.headers().contains_key(WORKSPACE_ID_HEADER));

        let owned = app
            .oneshot(
                Request::builder()
                    .uri("/owned")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(owned.headers()[REQUEST_ID_HEADER], "req_downstream");
        assert_eq!(owned.headers()[WORKSPACE_ID_HEADER], "workspace_downstream");

        for unusable in ["", "  \t", "workspace\ninvalid"] {
            let response = with_managed_workspace_header(StatusCode::OK.into_response(), unusable);
            assert!(!response.headers().contains_key(WORKSPACE_ID_HEADER));
        }
    }
}
