//! Awaken-only automatic Dream policy authoring. Anthropic's Managed Dream API
//! owns individual jobs; recurring policy is an Awaken control-plane extension.

use std::sync::Arc;

use awaken_session_contract::{
    DreamPolicy, DreamPolicyApplication, DreamPolicyApplicationError, DreamPolicyConfig,
};
use awaken_tenancy::WorkspaceScope;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Serialize;

pub fn dream_policy_router(application: Arc<dyn DreamPolicyApplication>) -> Router {
    Router::new()
        .route(
            "/v1/awaken/memory-stores/{id}/dream-policy",
            get(get_policy).put(put_policy),
        )
        .with_state(application)
}

#[derive(Debug, Serialize)]
struct DreamPolicyErrorBody {
    error: String,
}

type DreamPolicyRejection = (StatusCode, Json<DreamPolicyErrorBody>);

fn request_scope(scope: Option<Extension<WorkspaceScope>>) -> Result<String, DreamPolicyRejection> {
    match scope {
        Some(Extension(scope)) if !scope.0.trim().is_empty() => Ok(scope.0),
        _ => Err((
            StatusCode::NOT_FOUND,
            Json(DreamPolicyErrorBody {
                error: "workspace not found".into(),
            }),
        )),
    }
}

async fn get_policy(
    State(application): State<Arc<dyn DreamPolicyApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(memory_store_id): Path<String>,
) -> Result<Json<DreamPolicy>, DreamPolicyRejection> {
    application
        .policy(&request_scope(scope)?, &memory_store_id)
        .map(Json)
        .map_err(map_error)
}

async fn put_policy(
    State(application): State<Arc<dyn DreamPolicyApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(memory_store_id): Path<String>,
    Json(config): Json<DreamPolicyConfig>,
) -> Result<Json<DreamPolicy>, DreamPolicyRejection> {
    let workspace = request_scope(scope)?;
    application
        .set_policy(&workspace, &memory_store_id, config)
        .and_then(|()| application.policy(&workspace, &memory_store_id))
        .map(Json)
        .map_err(map_error)
}

fn map_error(error: DreamPolicyApplicationError) -> DreamPolicyRejection {
    let status = match &error {
        DreamPolicyApplicationError::BadRequest(_) => StatusCode::BAD_REQUEST,
        DreamPolicyApplicationError::NotFound => StatusCode::NOT_FOUND,
        DreamPolicyApplicationError::Conflict(_) => StatusCode::CONFLICT,
        DreamPolicyApplicationError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        status,
        Json(DreamPolicyErrorBody {
            error: error.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_scope_is_required_and_error_mapping_is_explicit() {
        assert_eq!(request_scope(None).unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(
            request_scope(Some(Extension(WorkspaceScope(String::new()))))
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request_scope(Some(Extension(WorkspaceScope("workspace-a".into())))).unwrap(),
            "workspace-a"
        );

        assert_eq!(
            map_error(DreamPolicyApplicationError::BadRequest("bad".into())).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            map_error(DreamPolicyApplicationError::NotFound).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_error(DreamPolicyApplicationError::Conflict("stale".into())).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_error(DreamPolicyApplicationError::Unavailable("offline".into())).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
