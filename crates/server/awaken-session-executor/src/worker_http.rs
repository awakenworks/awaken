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
pub fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                HostErrorKind::Conflict => StatusCode::CONFLICT,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect decision table for the sole HostError HTTP mapper.
    /// Causes are C1 success, C2 bad request, C3 conflict, and C4 internal
    /// failure. Effects are E1 200 with the original value, E2 400, E3 409,
    /// and E4 500; every error effect preserves the message in `{error}`.
    /// Rules H1 C1=>E1, H2 C2=>E2, H3 C3=>E3, H4 C4=>E4 exhaust the enum.
    #[test]
    fn host_error_http_mapping_exhausts_success_and_failure_classes() {
        let (status, Json(body)) = respond(Ok(json!({ "ok": true })));
        assert_eq!(status, StatusCode::OK, "H1");
        assert_eq!(body, json!({ "ok": true }), "H1");

        for (rule, error, expected) in [
            ("H2", HostError::bad_request("bad"), StatusCode::BAD_REQUEST),
            ("H3", HostError::conflict("conflict"), StatusCode::CONFLICT),
            (
                "H4",
                HostError::internal("internal"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let message = error.message.clone();
            let (status, Json(body)) = respond(Err(error));
            assert_eq!(status, expected, "{rule}");
            assert_eq!(body, json!({ "error": message }), "{rule}");
        }
    }
}
