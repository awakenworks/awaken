// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Static owner: the management identity edge and no-login composition. Dynamic path:
// @formal reason: the canonical PrincipalRef becomes the Resources MemoryActor; the authenticated or
// @formal reason: installed WorkspaceScope reaches authoring and response projection without
// @formal reason: a handler inventing, authorizing, or storing another identity. Evidence:
// @formal reason: runtime.permission.authority_never_widens; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Anthropic Managed response/request context at the management edge.
//! IAM owns authenticated identity; no-login composition owns its installed
//! Workspace. This module projects only their typed, non-secret context.

use awaken_iam_contract::PrincipalRef;
use axum::Router;

pub(super) fn memory_actor(
    principal: &PrincipalRef,
) -> awaken_protocol_managed::types::memory::AuthenticatedMemoryActor {
    use awaken_resource_contract::MemoryActor;
    let actor = match principal {
        PrincipalRef::Account { account_id } => MemoryActor::UserActor {
            user_id: account_id.0.clone(),
        },
        PrincipalRef::Service { service_id } => MemoryActor::ServiceAccountActor {
            service_account_id: service_id.clone(),
        },
        PrincipalRef::ApiToken { token_id } => MemoryActor::ApiActor {
            api_key_id: token_id.clone(),
        },
    };
    awaken_protocol_managed::types::memory::AuthenticatedMemoryActor(actor)
}

/// The no-login composition stamps its installed Workspace on the entire
/// management request before audit, authoring and response projection run.
/// An already authenticated scope remains authoritative; no handler invents one.
pub(crate) fn with_composed_workspace(router: Router, workspace: Option<String>) -> Router {
    let Some(workspace) = workspace.filter(|value| !value.trim().is_empty()) else {
        return router;
    };
    router.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let workspace = workspace.clone();
            async move {
                if request
                    .extensions()
                    .get::<awaken_tenancy::WorkspaceScope>()
                    .is_none()
                {
                    request
                        .extensions_mut()
                        .insert(awaken_tenancy::WorkspaceScope(workspace));
                }
                next.run(request).await
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use awaken_iam_contract::{AccountId, PrincipalRef};
    use awaken_resource_contract::MemoryActor;
    use awaken_tenancy::WorkspaceScope;
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        extract::Extension,
        http::{Request, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use serde_json::json;
    use tower::ServiceExt;

    use super::{memory_actor, with_composed_workspace};

    #[tokio::test]
    async fn composed_workspace_scopes_every_management_route_before_response_projection() {
        // Causes: C1 no-login has an installed Workspace; C2 an authenticated
        // scope already exists; C3 no authority supplies a Workspace. Effects:
        // E1 both authoring and context routes see C1 and project its header;
        // E2 C2 is preserved; E3 C3 reaches neither a fabricated scope nor a
        // Workspace header. Decision rules: C1&&!C2=>E1, C2=>E2, !C1&&!C2=>E3.
        async fn probe(scope: Option<Extension<WorkspaceScope>>) -> impl IntoResponse {
            match scope {
                Some(Extension(scope)) => (StatusCode::OK, Json(json!({"workspace_id": scope.0}))),
                None => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "workspace missing"})),
                ),
            }
        }
        for (route, configured, incoming, status, expected_workspace) in [
            (
                "/v1/config/provider-connections",
                Some("workspace-local"),
                None,
                StatusCode::OK,
                Some("workspace-local"),
            ),
            (
                "/v1/config/workspace-context",
                Some("workspace-local"),
                None,
                StatusCode::OK,
                Some("workspace-local"),
            ),
            (
                "/v1/config/provider-connections",
                Some("workspace-local"),
                Some("workspace-iam"),
                StatusCode::OK,
                Some("workspace-iam"),
            ),
            (
                "/v1/config/provider-connections",
                None,
                None,
                StatusCode::NOT_FOUND,
                None,
            ),
        ] {
            let app = with_composed_workspace(
                awaken_protocol_managed::with_managed_response_context(
                    Router::new()
                        .route("/v1/config/provider-connections", get(probe))
                        .route("/v1/config/workspace-context", get(probe)),
                ),
                configured.map(str::to_owned),
            );
            let mut request = Request::get(route).body(Body::empty()).unwrap();
            if let Some(incoming) = incoming {
                request
                    .extensions_mut()
                    .insert(WorkspaceScope(incoming.to_owned()));
            }
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), status, "{route}");
            assert_eq!(
                response
                    .headers()
                    .get("anthropic-workspace-id")
                    .and_then(|value| value.to_str().ok()),
                expected_workspace,
                "{route}",
            );
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if let Some(workspace) = expected_workspace {
                assert_eq!(body["workspace_id"], workspace, "{route}");
            }
        }
    }

    #[test]
    fn iam_principals_lower_once_to_managed_memory_actors() {
        // Cause/effect graph: C1 API token, C2 human account, C3 service account.
        // Effects: E1 api_actor, E2 user_actor, E3 service_account_actor with the
        // original non-secret identifier. Rules R1=C1->E1, R2=C2->E2, R3=C3->E3.
        // The IAM principal stays authoritative; this is only its protocol context.
        let cases = [
            (
                PrincipalRef::ApiToken {
                    token_id: "api_1".into(),
                },
                MemoryActor::ApiActor {
                    api_key_id: "api_1".into(),
                },
            ),
            (
                PrincipalRef::Account {
                    account_id: AccountId("user_1".into()),
                },
                MemoryActor::UserActor {
                    user_id: "user_1".into(),
                },
            ),
            (
                PrincipalRef::Service {
                    service_id: "svac_1".into(),
                },
                MemoryActor::ServiceAccountActor {
                    service_account_id: "svac_1".into(),
                },
            ),
        ];
        for (principal, expected) in cases {
            assert_eq!(memory_actor(&principal).0, expected);
        }
    }
}
