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
use axum::{Json, Router};
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

/// The Workspace the trusted ingress edge already selected. Absence is refused
/// here, as an extractor, so no handler body below has an absent case to give a
/// default meaning to.
struct RequiredWorkspaceScope(String);

impl<S> axum::extract::FromRequestParts<S> for RequiredWorkspaceScope
where
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        match parts
            .extensions
            .get::<WorkspaceScope>()
            .and_then(WorkspaceScope::non_empty)
        {
            Some(scope) => Ok(Self(scope.to_owned())),
            None => Err(axum::response::IntoResponse::into_response((
                StatusCode::NOT_FOUND,
                Json(DreamPolicyErrorBody {
                    error: "workspace not found".into(),
                }),
            ))),
        }
    }
}

async fn get_policy(
    State(application): State<Arc<dyn DreamPolicyApplication>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(memory_store_id): Path<String>,
) -> Result<Json<DreamPolicy>, DreamPolicyRejection> {
    application
        .policy(&workspace, &memory_store_id)
        .map(Json)
        .map_err(map_error)
}

async fn put_policy(
    State(application): State<Arc<dyn DreamPolicyApplication>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(memory_store_id): Path<String>,
    Json(config): Json<DreamPolicyConfig>,
) -> Result<Json<DreamPolicy>, DreamPolicyRejection> {
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

    /// Exercise the extractor the way axum does, so absence is tested where it
    /// is actually decided rather than through a deleted helper function.
    async fn extract_scope(stamp: Option<&str>) -> Result<String, StatusCode> {
        use axum::extract::FromRequestParts;
        let mut parts = axum::http::Request::new(()).into_parts().0;
        if let Some(stamp) = stamp {
            parts.extensions.insert(WorkspaceScope(stamp.to_string()));
        }
        RequiredWorkspaceScope::from_request_parts(&mut parts, &())
            .await
            .map(|RequiredWorkspaceScope(scope)| scope)
            .map_err(|rejection| {
                use axum::response::IntoResponse;
                rejection.into_response().status()
            })
    }

    #[tokio::test]
    async fn workspace_scope_is_required_and_error_mapping_is_explicit() {
        assert_eq!(
            extract_scope(None).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            extract_scope(Some("")).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            extract_scope(Some("workspace-a")).await.unwrap(),
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
