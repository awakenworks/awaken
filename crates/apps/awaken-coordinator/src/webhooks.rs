//! The webhook plane (ADR-0048 / S10). `awaken-webhook` owns lifecycle delivery;
//! Coordinator owns the guarded request-scope adapter because only its ingress
//! can interpret Control's authenticated tenancy.
pub use awaken_webhook::lifecycle::*;

use awaken_authz_enforce::RequestTenancy;
use awaken_runtime_contract::runs::scope::WorkspaceScope;

/// Map guard-resolved tenancy onto the protocol-neutral Workspace scope used by
/// Coordinator handlers. Apply this layer inside the guard.
pub async fn stamp_workspace_scope(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(tenancy) = request.extensions().get::<RequestTenancy>().cloned() {
        request
            .extensions_mut()
            .insert(WorkspaceScope(tenancy.workspace_id));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::{Extension, Router};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn guarded_tenancy_is_the_authority_for_workspace_scope() {
        // Causes: C1 the guard resolved RequestTenancy; C2 an earlier layer
        // installed WorkspaceScope. Effects: E1 C1 stamps the guard's owner,
        // replacing a conflicting C2; E2 !C1 leaves C2 unchanged; E3 neither
        // cause leaves the request unscoped. Decision rows R1=C1+!C2=>E1,
        // R2=C1+C2=>E1, R3=!C1+C2=>E2, R4=!C1+!C2=>E3.
        async fn probe(scope: Option<Extension<WorkspaceScope>>) -> String {
            scope
                .map(|Extension(WorkspaceScope(workspace))| workspace)
                .unwrap_or_else(|| "<none>".into())
        }
        let app = Router::new()
            .route("/p", get(probe))
            .layer(axum::middleware::from_fn(stamp_workspace_scope));
        for (rule, owner, previous, expected) in [
            ("R1", Some("ws_x"), None, "ws_x"),
            ("R2", Some("ws_x"), Some("stale"), "ws_x"),
            ("R3", None, Some("existing"), "existing"),
            ("R4", None, None, "<none>"),
        ] {
            let mut request = Request::builder().uri("/p").body(Body::empty()).unwrap();
            if let Some(workspace) = owner {
                request.extensions_mut().insert(RequestTenancy {
                    workspace_id: workspace.into(),
                });
            }
            if let Some(workspace) = previous {
                request
                    .extensions_mut()
                    .insert(WorkspaceScope(workspace.into()));
            }
            let response = app.clone().oneshot(request).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], expected.as_bytes(), "{rule}");
        }
    }
}
