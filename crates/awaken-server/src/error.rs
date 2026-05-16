//! Canonical API error type for HTTP handlers.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// API error type returned by route handlers.
///
/// Marked `#[non_exhaustive]` so adding variants is not a SemVer-breaking
/// change for downstream crates that match on it.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    Conflict(String),
    NotFound(String),
    ThreadNotFound(String),
    RunNotFound(String),
    /// Feature/route present but the backing capability isn't installed —
    /// e.g. `permission-preview` requested but server was built without
    /// `--features permission`. Distinct from 404 so callers can
    /// differentiate "no such resource" from "feature not available".
    ServiceUnavailable(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::ThreadNotFound(id) => {
                (StatusCode::NOT_FOUND, format!("thread not found: {id}"))
            }
            ApiError::RunNotFound(id) => (StatusCode::NOT_FOUND, format!("run not found: {id}")),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}
