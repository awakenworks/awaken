//! Canonical API error type for HTTP handlers.

use std::fmt;

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

impl fmt::Display for ApiError {
    // Mirrors the user-facing message picked by `IntoResponse`. Lets call
    // sites embed an `ApiError` in another diagnostic via `{err}` without
    // leaking the variant name / `Debug` formatting into user-visible
    // payloads (e.g. per-cell `ReplayRuntimeFailure::RuntimeError.message`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::BadRequest(msg)
            | ApiError::Unauthorized(msg)
            | ApiError::Conflict(msg)
            | ApiError::NotFound(msg)
            | ApiError::ServiceUnavailable(msg)
            | ApiError::Internal(msg) => f.write_str(msg),
            ApiError::ThreadNotFound(id) => write!(f, "thread not found: {id}"),
            ApiError::RunNotFound(id) => write!(f, "run not found: {id}"),
        }
    }
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
