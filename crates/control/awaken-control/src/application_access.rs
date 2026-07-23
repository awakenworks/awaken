//! Management API for issuing narrow, short-lived application credentials.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_authz_enforce::{ApplicationAccessStore, ApplicationGrant};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const DEFAULT_TTL_SECONDS: u64 = 300;
const MAX_TTL_SECONDS: u64 = 900;

#[derive(Debug, Deserialize)]
pub struct CreateApplicationToken {
    pub authority_id: String,
    pub application_scope: String,
    #[serde(default = "default_thread_namespace")]
    pub thread_namespace: String,
    #[serde(default)]
    pub actor_key: Option<String>,
    #[serde(default = "default_operations")]
    pub operations: Vec<String>,
    pub agent_ids: Vec<String>,
    #[serde(default)]
    pub default_agent_id: Option<String>,
    #[serde(default = "default_ttl")]
    pub expires_in_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct IssuedApplicationToken {
    pub id: String,
    pub object: &'static str,
    pub token_type: &'static str,
    pub access_token: String,
    pub expires_at: String,
    pub application_scope: String,
    pub thread_namespace: String,
}

#[derive(Clone)]
struct ApplicationTokenState {
    store: Arc<ApplicationAccessStore>,
    default_workspace: String,
}

pub fn router(store: Arc<ApplicationAccessStore>, default_workspace: String) -> Router {
    Router::new()
        .route("/v1/application-access-tokens", post(create))
        .route("/v1/application-access-tokens/{id}", delete(revoke))
        .with_state(ApplicationTokenState {
            store,
            default_workspace,
        })
}

async fn create(
    State(state): State<ApplicationTokenState>,
    workspace: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Json(request): Json<CreateApplicationToken>,
) -> Response {
    if let Err(detail) = validate(&request) {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, detail);
    }
    let workspace_id = workspace
        .map(|Extension(scope)| scope.0)
        .unwrap_or(state.default_workspace);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let expires_ms = now_ms.saturating_add(request.expires_in_seconds.saturating_mul(1000));
    let expires_at = awaken_protocol_managed::cron::to_rfc3339(expires_ms);
    let id = format!(
        "aat_{}_{}",
        now_ms,
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let grant = ApplicationGrant {
        authority_id: request.authority_id,
        application_scope: request.application_scope.clone(),
        thread_namespace: request.thread_namespace.clone(),
        actor_key: request.actor_key,
        operations: request.operations.iter().cloned().collect::<HashSet<_>>(),
        agent_ids: request.agent_ids.iter().cloned().collect::<HashSet<_>>(),
        default_agent_id: request.default_agent_id,
    };
    match state
        .store
        .mint(id.clone(), workspace_id, Some(expires_at.clone()), grant)
    {
        Ok(access_token) => (
            StatusCode::CREATED,
            Json(IssuedApplicationToken {
                id,
                object: "application_access_token",
                token_type: "Bearer",
                access_token,
                expires_at,
                application_scope: request.application_scope,
                thread_namespace: request.thread_namespace,
            }),
        )
            .into_response(),
        Err(error) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not issue application token: {error}"),
        ),
    }
}

async fn revoke(State(state): State<ApplicationTokenState>, Path(id): Path<String>) -> Response {
    match state.store.revoke(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => problem(StatusCode::NOT_FOUND, "application token not found"),
    }
}

fn validate(request: &CreateApplicationToken) -> Result<(), &'static str> {
    if request.authority_id.trim().is_empty()
        || request.application_scope.trim().is_empty()
        || request.thread_namespace.trim().is_empty()
    {
        return Err("authority_id, application_scope, and thread_namespace are required");
    }
    if request.expires_in_seconds == 0 || request.expires_in_seconds > MAX_TTL_SECONDS {
        return Err("expires_in_seconds must be between 1 and 900");
    }
    if request.agent_ids.is_empty() || request.agent_ids.iter().any(|id| id.trim().is_empty()) {
        return Err("agent_ids must contain at least one non-empty Agent id");
    }
    if let Some(default_agent) = &request.default_agent_id
        && !request.agent_ids.contains(default_agent)
    {
        return Err("default_agent_id must be present in agent_ids");
    }
    if request.operations.is_empty()
        || request
            .operations
            .iter()
            .any(|operation| !matches!(operation.as_str(), "thread.run" | "thread.read"))
    {
        return Err("operations may contain only thread.run and thread.read");
    }
    Ok(())
}

fn problem(status: StatusCode, detail: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "type": "about:blank",
            "title": status.canonical_reason().unwrap_or("Application access error"),
            "status": status.as_u16(),
            "detail": detail.into(),
        })),
    )
        .into_response()
}

fn default_thread_namespace() -> String {
    "default".to_string()
}

fn default_operations() -> Vec<String> {
    vec!["thread.run".to_string(), "thread.read".to_string()]
}

const fn default_ttl() -> u64 {
    DEFAULT_TTL_SECONDS
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn issues_a_token_with_the_requested_opaque_scope() {
        let store = Arc::new(ApplicationAccessStore::new());
        let response = router(store.clone(), "workspace-a".to_string())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/application-access-tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "authority_id": "shop-backend",
                            "application_scope": "project-42",
                            "thread_namespace": "customer-chat",
                            "agent_ids": ["support"],
                            "default_agent_id": "support"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let token = body["access_token"].as_str().unwrap();
        let identity = store.authenticate(token).unwrap();
        assert_eq!(identity.workspace_id, "workspace-a");
        assert_eq!(identity.grant.application_scope, "project-42");
        assert!(identity.grant.operations.contains("thread.run"));
        assert!(identity.grant.operations.contains("thread.read"));
    }

    #[tokio::test]
    async fn rejects_an_agent_default_outside_the_allow_list() {
        let response = router(
            Arc::new(ApplicationAccessStore::new()),
            "workspace-a".to_string(),
        )
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/application-access-tokens")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "authority_id": "shop-backend",
                        "application_scope": "project-42",
                        "agent_ids": ["support"],
                        "default_agent_id": "billing"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
