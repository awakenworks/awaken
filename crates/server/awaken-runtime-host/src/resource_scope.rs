//! Resource-route extraction of the Workspace selected by the composition edge.
//!
//! Authentication and authorization happen before these routes. Local mode also
//! selects its hidden default Workspace at the composition root. Resource APIs
//! therefore consume an existing [`WorkspaceScope`] and never infer one from the
//! Host, credentials, route, or deployment mode.

use awaken_tenancy::WorkspaceScope;
use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};

/// Workspace selected and stamped by the trusted composition edge.
///
/// Missing and empty values fail as `404` so an incorrectly wired route cannot
/// create an unscoped resource or disclose whether a resource id exists.
#[derive(Debug)]
pub struct RequiredWorkspaceScope(pub String);

impl<S> FromRequestParts<S> for RequiredWorkspaceScope
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<WorkspaceScope>() {
            Some(scope) if !scope.0.trim().is_empty() => Ok(Self(scope.0.clone())),
            _ => Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "workspace not found" })),
            )
                .into_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    #[tokio::test]
    async fn requires_a_non_empty_preselected_workspace() {
        let (mut missing, mut empty, mut selected) = (
            Request::new(Body::empty()).into_parts().0,
            Request::new(Body::empty()).into_parts().0,
            Request::new(Body::empty()).into_parts().0,
        );
        empty.extensions.insert(WorkspaceScope(String::new()));
        selected
            .extensions
            .insert(WorkspaceScope("workspace-a".into()));

        assert_eq!(
            RequiredWorkspaceScope::from_request_parts(&mut missing, &())
                .await
                .unwrap_err()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            RequiredWorkspaceScope::from_request_parts(&mut empty, &())
                .await
                .unwrap_err()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            RequiredWorkspaceScope::from_request_parts(&mut selected, &())
                .await
                .unwrap()
                .0,
            "workspace-a"
        );
    }
}
