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
    Gone(String),
    NotFound(String),
    ThreadNotFound(String),
    RunNotFound(String),
    /// Feature/route present but the backing capability isn't installed —
    /// e.g. `permission-preview` requested but server was built without
    /// `--features permission`. Distinct from 404 so callers can
    /// differentiate "no such resource" from "feature not available".
    ServiceUnavailable(String),
    Internal(String),
    /// Storage promised a row that is missing inside retention. Distinct from
    /// `Internal` so operators can alert on integrity violations (ADR-0034 D8:
    /// "integrity error and metric; do not silently re-run the projector").
    DataIntegrity(String),
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
            | ApiError::Gone(msg)
            | ApiError::NotFound(msg)
            | ApiError::ServiceUnavailable(msg)
            | ApiError::Internal(msg)
            | ApiError::DataIntegrity(msg) => f.write_str(msg),
            ApiError::ThreadNotFound(id) => write!(f, "thread not found: {id}"),
            ApiError::RunNotFound(id) => write!(f, "run not found: {id}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg, None),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg, None),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg, None),
            ApiError::Gone(msg) => (StatusCode::GONE, msg, None),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg, None),
            ApiError::ThreadNotFound(id) => (
                StatusCode::NOT_FOUND,
                format!("thread not found: {id}"),
                None,
            ),
            ApiError::RunNotFound(id) => {
                (StatusCode::NOT_FOUND, format!("run not found: {id}"), None)
            }
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg, None),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg, None),
            ApiError::DataIntegrity(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                msg,
                Some("data_integrity"),
            ),
        };
        let body = match code {
            Some(code) => json!({ "error": message, "code": code }),
            None => json!({ "error": message }),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn data_integrity_carries_distinct_code_for_operators() {
        let response = ApiError::DataIntegrity("row missing".into()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "row missing");
        assert_eq!(value["code"], "data_integrity");
    }

    #[tokio::test]
    async fn internal_does_not_carry_code_field() {
        let response = ApiError::Internal("boom".into()).into_response();
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "boom");
        assert!(value.get("code").is_none());
    }
}
