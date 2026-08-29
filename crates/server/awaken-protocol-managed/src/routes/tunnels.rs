//! Public Tunnel ACL over the Cloud-only [`ManagedTunnelApplication`] port.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::{ManagedJson, ManagedQuery, WorkspaceScope};
use crate::types::tunnel::{
    CertificateCreateParams, TunnelCreateParams, TunnelListQuery, TunnelRotateTokenParams,
};
use crate::types::{ErrorResponse, PageCursor, paginate};
use crate::{ManagedTunnelApplication, ManagedTunnelApplicationError, ManagedTunnelScope};

type WireError = (StatusCode, Json<ErrorResponse>);

fn scope(workspace: Option<Extension<WorkspaceScope>>, headers: &HeaderMap) -> ManagedTunnelScope {
    ManagedTunnelScope {
        workspace_id: workspace
            .map(|scope| scope.0.0)
            .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_owned()),
        operation_id: headers
            .get("x-request-id")
            .or_else(|| headers.get("idempotency-key"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    }
}

fn wire_error(error: ManagedTunnelApplicationError) -> WireError {
    match error {
        ManagedTunnelApplicationError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found_error", error.to_string())),
        ),
        ManagedTunnelApplicationError::Invalid(_) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                error.to_string(),
            )),
        ),
        ManagedTunnelApplicationError::Conflict(_) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "invalid_request_error",
                error.to_string(),
            )),
        ),
        ManagedTunnelApplicationError::Forbidden => (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new("permission_error", error.to_string())),
        ),
        ManagedTunnelApplicationError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", error.to_string())),
        ),
    }
}

pub fn tunnels_router(application: Arc<dyn ManagedTunnelApplication>) -> Router {
    Router::new()
        .route("/v1/tunnels", post(create).get(list))
        .route("/v1/tunnels/{tunnel_id}", get(retrieve))
        .route("/v1/tunnels/{tunnel_id}/archive", post(archive))
        .route("/v1/tunnels/{tunnel_id}/reveal_token", post(reveal_token))
        .route("/v1/tunnels/{tunnel_id}/rotate_token", post(rotate_token))
        .route(
            "/v1/tunnels/{tunnel_id}/certificates",
            post(create_certificate).get(list_certificates),
        )
        .route(
            "/v1/tunnels/{tunnel_id}/certificates/{certificate_id}",
            get(retrieve_certificate),
        )
        .route(
            "/v1/tunnels/{tunnel_id}/certificates/{certificate_id}/archive",
            post(archive_certificate),
        )
        // Deprecated Admin API aliases. They drive the same aggregate so the
        // migration window never dual-writes or forks token/certificate state.
        .route("/v1/organizations/tunnels", post(create).get(list))
        .route("/v1/organizations/tunnels/{tunnel_id}", get(retrieve))
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/archive",
            post(archive),
        )
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/reveal_token",
            post(reveal_token),
        )
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/rotate_token",
            post(rotate_token),
        )
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/certificates",
            post(create_certificate).get(list_certificates),
        )
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/certificates/{certificate_id}",
            get(retrieve_certificate),
        )
        .route(
            "/v1/organizations/tunnels/{tunnel_id}/certificates/{certificate_id}/archive",
            post(archive_certificate),
        )
        .with_state(application)
}

async fn create(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<TunnelCreateParams>,
) -> Result<Json<crate::types::tunnel::Tunnel>, WireError> {
    if params
        .display_name
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.chars().count() > 255)
    {
        return Err(wire_error(ManagedTunnelApplicationError::Invalid(
            "display_name must contain 1-255 characters".into(),
        )));
    }
    app.create_tunnel(scope(workspace, &headers), params.display_name)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn retrieve(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
) -> Result<Json<crate::types::tunnel::Tunnel>, WireError> {
    app.retrieve_tunnel(scope(workspace, &headers), &tunnel_id)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn list(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedQuery(query): ManagedQuery<TunnelListQuery>,
) -> Result<Json<PageCursor<crate::types::tunnel::Tunnel>>, WireError> {
    let rows = app
        .list_tunnels(scope(workspace, &headers), query.include_archived)
        .await
        .map_err(wire_error)?;
    Ok(Json(paginate(rows, &query.page, |row| &row.id)))
}

async fn archive(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
) -> Result<Json<crate::types::tunnel::Tunnel>, WireError> {
    app.archive_tunnel(scope(workspace, &headers), &tunnel_id)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn reveal_token(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<crate::types::tunnel::TunnelToken>), WireError> {
    let token = app
        .reveal_token(scope(workspace, &headers), &tunnel_id)
        .await
        .map_err(wire_error)?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert("cache-control", "no-store".parse().unwrap());
    Ok((response_headers, Json(token)))
}

async fn rotate_token(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<TunnelRotateTokenParams>,
) -> Result<(HeaderMap, Json<crate::types::tunnel::TunnelToken>), WireError> {
    let token = app
        .rotate_token(scope(workspace, &headers), &tunnel_id, params.reason)
        .await
        .map_err(wire_error)?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert("cache-control", "no-store".parse().unwrap());
    Ok((response_headers, Json(token)))
}

async fn create_certificate(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<CertificateCreateParams>,
) -> Result<Json<crate::types::tunnel::TunnelCertificate>, WireError> {
    if params.ca_certificate_pem.len() > 8 * 1024 {
        return Err(wire_error(ManagedTunnelApplicationError::Invalid(
            "ca_certificate_pem exceeds 8KB".into(),
        )));
    }
    app.create_certificate(
        scope(workspace, &headers),
        &tunnel_id,
        params.ca_certificate_pem,
    )
    .await
    .map(Json)
    .map_err(wire_error)
}

async fn retrieve_certificate(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path((tunnel_id, certificate_id)): Path<(String, String)>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
) -> Result<Json<crate::types::tunnel::TunnelCertificate>, WireError> {
    app.retrieve_certificate(scope(workspace, &headers), &tunnel_id, &certificate_id)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn list_certificates(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path(tunnel_id): Path<String>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedQuery(query): ManagedQuery<TunnelListQuery>,
) -> Result<Json<PageCursor<crate::types::tunnel::TunnelCertificate>>, WireError> {
    let rows = app
        .list_certificates(
            scope(workspace, &headers),
            &tunnel_id,
            query.include_archived,
        )
        .await
        .map_err(wire_error)?;
    Ok(Json(paginate(rows, &query.page, |row| &row.id)))
}

async fn archive_certificate(
    State(app): State<Arc<dyn ManagedTunnelApplication>>,
    Path((tunnel_id, certificate_id)): Path<(String, String)>,
    workspace: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
) -> Result<Json<crate::types::tunnel::TunnelCertificate>, WireError> {
    app.archive_certificate(scope(workspace, &headers), &tunnel_id, &certificate_id)
        .await
        .map(Json)
        .map_err(wire_error)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use axum::response::Response;
    use tower::ServiceExt;

    use super::*;
    use crate::types::tunnel::{Tunnel, TunnelCertificate, TunnelToken};

    #[derive(Default)]
    struct FakeTunnelApplication {
        scopes: Mutex<Vec<ManagedTunnelScope>>,
    }

    impl FakeTunnelApplication {
        fn tunnel(&self, id: &str) -> Tunnel {
            Tunnel {
                id: id.into(),
                kind: "tunnel".into(),
                archived_at: None,
                created_at: "2026-08-12T00:00:00Z".into(),
                display_name: Some("docs".into()),
                domain: format!("{id}.tunnels.example"),
            }
        }

        fn certificate(&self, tunnel_id: &str, id: &str) -> TunnelCertificate {
            TunnelCertificate {
                id: id.into(),
                kind: "tunnel_certificate".into(),
                archived_at: None,
                created_at: "2026-08-12T00:00:00Z".into(),
                expires_at: None,
                fingerprint: "sha256:test".into(),
                tunnel_id: tunnel_id.into(),
            }
        }

        fn record(&self, scope: ManagedTunnelScope) {
            self.scopes.lock().unwrap().push(scope);
        }
    }

    #[async_trait::async_trait]
    impl ManagedTunnelApplication for FakeTunnelApplication {
        async fn create_tunnel(
            &self,
            scope: ManagedTunnelScope,
            _: Option<String>,
        ) -> Result<Tunnel, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(self.tunnel("tun_1"))
        }
        async fn retrieve_tunnel(
            &self,
            scope: ManagedTunnelScope,
            id: &str,
        ) -> Result<Tunnel, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(self.tunnel(id))
        }
        async fn list_tunnels(
            &self,
            scope: ManagedTunnelScope,
            _: bool,
        ) -> Result<Vec<Tunnel>, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(vec![self.tunnel("tun_1")])
        }
        async fn archive_tunnel(
            &self,
            scope: ManagedTunnelScope,
            id: &str,
        ) -> Result<Tunnel, ManagedTunnelApplicationError> {
            self.record(scope);
            let mut tunnel = self.tunnel(id);
            tunnel.archived_at = Some("2026-08-12T00:00:01Z".into());
            Ok(tunnel)
        }
        async fn reveal_token(
            &self,
            scope: ManagedTunnelScope,
            id: &str,
        ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(TunnelToken {
                id: id.into(),
                kind: "tunnel_token".into(),
                tunnel_token: "secret".into(),
            })
        }
        async fn rotate_token(
            &self,
            scope: ManagedTunnelScope,
            id: &str,
            _: Option<String>,
        ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
            self.reveal_token(scope, id).await
        }
        async fn create_certificate(
            &self,
            scope: ManagedTunnelScope,
            tunnel: &str,
            _: String,
        ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(self.certificate(tunnel, "tcrt_1"))
        }
        async fn retrieve_certificate(
            &self,
            scope: ManagedTunnelScope,
            tunnel: &str,
            id: &str,
        ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(self.certificate(tunnel, id))
        }
        async fn list_certificates(
            &self,
            scope: ManagedTunnelScope,
            tunnel: &str,
            _: bool,
        ) -> Result<Vec<TunnelCertificate>, ManagedTunnelApplicationError> {
            self.record(scope);
            Ok(vec![self.certificate(tunnel, "tcrt_1")])
        }
        async fn archive_certificate(
            &self,
            scope: ManagedTunnelScope,
            tunnel: &str,
            id: &str,
        ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
            self.record(scope);
            let mut certificate = self.certificate(tunnel, id);
            certificate.archived_at = Some("2026-08-12T00:00:01Z".into());
            Ok(certificate)
        }
    }

    async fn call(app: &Router, method: &str, uri: &str, body: &'static str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn tunnel_acl_follows_scope_secret_and_validation_decision_table() {
        // Causes: C1=trusted Workspace extension, C2=request id, C3=token
        // response, C4=certificate <=8KiB. Effects: E1=scope reaches the sole
        // Cloud port, E2=token is non-cacheable, E3=valid command reaches port,
        // E4=oversized PEM is rejected before port. Decision rules:
        // R1=C1+C2+C3 -> E1+E2; R2=C1+C4 -> E3; R3=!C4 -> E4.
        let application = Arc::new(FakeTunnelApplication::default());
        let app = tunnels_router(application.clone())
            .layer(Extension(WorkspaceScope("workspace-a".into())));

        let token = app
            .clone()
            .oneshot(
                Request::post("/v1/tunnels/tun_1/reveal_token")
                    .header("x-request-id", "request-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token.status(), StatusCode::OK, "R1");
        assert_eq!(token.headers()["cache-control"], "no-store", "R1/E2");
        let token_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(token.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(token_json["tunnel_token"], "secret", "R1");

        let valid = app
            .clone()
            .oneshot(
                Request::post("/v1/tunnels/tun_1/certificates")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ca_certificate_pem":"pem"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::OK, "R2");

        let legacy = app
            .clone()
            .oneshot(
                Request::get("/v1/organizations/tunnels/tun_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::OK, "migration alias");
        let legacy_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(legacy.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(legacy_json["id"], "tun_1");

        let oversized = serde_json::json!({"ca_certificate_pem":"x".repeat(8193)}).to_string();
        let invalid = app
            .oneshot(
                Request::post("/v1/tunnels/tun_1/certificates")
                    .header("content-type", "application/json")
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST, "R3");
        let scopes = application.scopes.lock().unwrap();
        assert_eq!(scopes.len(), 3, "R3/E4 + migration alias");
        assert_eq!(scopes[0].workspace_id, "workspace-a", "R1/E1");
        assert_eq!(
            scopes[0].operation_id.as_deref(),
            Some("request-a"),
            "R1/E1"
        );
    }

    // Test design: tunnel_lifecycle_projects_every_current_sdk_operation
    // Cause/effect graph: Tunnel creation, token/certificate rotation, listing, retrieval and archive share one aggregate.
    // Decision table: active+owner=mutate/read; stale token/cert=conflict; archived=terminal; unknown/cross-owner=404.
    #[tokio::test]
    async fn tunnel_lifecycle_projects_every_current_sdk_operation() {
        // State-transition design T1: create, read/list, secret operations,
        // certificate lifecycle, and terminal archive all cross the same
        // Workspace-scoped application port. The decision chain covers every
        // current official Tunnel SDK method without duplicating Cloud effects.
        let application = Arc::new(FakeTunnelApplication::default());
        let app = tunnels_router(application.clone())
            .layer(Extension(WorkspaceScope("workspace-a".into())));
        for prefix in ["/v1/tunnels", "/v1/organizations/tunnels"] {
            let cases = [
                ("POST", prefix.to_owned(), r#"{"display_name":"test"}"#),
                ("GET", format!("{prefix}/tun_1"), ""),
                ("GET", prefix.to_owned(), ""),
                ("POST", format!("{prefix}/tun_1/reveal_token"), ""),
                (
                    "POST",
                    format!("{prefix}/tun_1/rotate_token"),
                    r#"{"reason":"test"}"#,
                ),
                (
                    "POST",
                    format!("{prefix}/tun_1/certificates"),
                    r#"{"ca_certificate_pem":"pem"}"#,
                ),
                ("GET", format!("{prefix}/tun_1/certificates/tcrt_1"), ""),
                ("GET", format!("{prefix}/tun_1/certificates"), ""),
                (
                    "POST",
                    format!("{prefix}/tun_1/certificates/tcrt_1/archive"),
                    "",
                ),
                ("POST", format!("{prefix}/tun_1/archive"), ""),
            ];
            for (method, uri, body) in cases {
                let response = call(&app, method, &uri, body).await;
                assert_eq!(response.status(), StatusCode::OK, "T1/{method} {uri}");
                if uri.ends_with("reveal_token") || uri.ends_with("rotate_token") {
                    assert_eq!(response.headers()["cache-control"], "no-store", "T1/secret");
                }
            }
        }
        assert_eq!(
            application.scopes.lock().unwrap().len(),
            20,
            "T1/one port call"
        );
    }
}
