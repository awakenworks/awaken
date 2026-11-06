// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Existing native scope/error or route assembly projection, without a second Session or authorization owner.
//! Common framing for Awaken-native JSON operations.
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExtensionErrorBody {
    r#type: &'static str,
    error: ExtensionError,
}
#[derive(Debug, serde::Serialize)]
struct ExtensionError {
    r#type: &'static str,
    message: String,
}
pub(crate) type ExtensionRejection = (StatusCode, Json<ExtensionErrorBody>);
pub(crate) fn extension_rejection(
    status: StatusCode,
    kind: &'static str,
    message: String,
) -> ExtensionRejection {
    (
        status,
        Json(ExtensionErrorBody {
            r#type: "error",
            error: ExtensionError {
                r#type: kind,
                message,
            },
        }),
    )
}
/// Missing/empty/foreign Workspace remains indistinguishable from absence.
pub(crate) struct RequiredWorkspaceScope(pub String);
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for RequiredWorkspaceScope {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<WorkspaceScope>()
            .and_then(WorkspaceScope::non_empty)
            .map(|scope| Self(scope.to_owned()))
            .ok_or_else(|| {
                extension_rejection(
                    StatusCode::NOT_FOUND,
                    "not_found_error",
                    "session not found".into(),
                )
                .into_response()
            })
    }
}
