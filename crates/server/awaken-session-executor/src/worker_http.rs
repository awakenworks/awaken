//! Shared HTTP mapping for the worker-facing router surfaces (dispatch transport,
//! commit ingest, durable ops). `HostError` itself stays transport-neutral — the
//! Managed/AI-SDK adapters map it to their own error shapes — but these three
//! worker seams share one plain `{error}` JSON mapping, so it lives here once.

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::host::{HostError, HostErrorKind};

/// Map a worker-router result to its HTTP response: `Ok` verbatim, `Err` to a
/// status by fault class plus a `{error}` body.
pub(crate) fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}
