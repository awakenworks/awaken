//! A2A response, fault, and streaming-envelope projection.

use std::convert::Infallible;

use awaken_runtime_contract::runs::{RunError, RunErrorClassification, RunErrorKind};

use axum::Json;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::state_error::rpc_error;
use crate::types::{ErrorResponse, StreamResponse};
use crate::v1::stream_value as v1_stream_value;
use crate::version::ProtocolVersion;

/// A JSON-RPC success: `{ jsonrpc, id, result }` on a 200.
pub(super) fn rpc_ok(id: Value, result: impl serde::Serialize) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// Map a driver error to a JSON-RPC `(code, message)` pair.
pub(super) fn rpc_fault(err: RunError) -> (i32, String) {
    let (_, code) = driver_fault(&err);
    (code, err.message)
}

fn driver_fault(error: &RunError) -> (StatusCode, i32) {
    match error.classification() {
        RunErrorClassification::NotFound => (StatusCode::NOT_FOUND, -32001),
        RunErrorClassification::UnsupportedOutputModes => (StatusCode::BAD_REQUEST, -32005),
        RunErrorClassification::InvalidRequest => (StatusCode::BAD_REQUEST, -32602),
        RunErrorClassification::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, -32603),
        RunErrorClassification::Conflict => (StatusCode::CONFLICT, -32603),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, -32603),
    }
}

pub(super) fn cancel_driver_error(error: RunError) -> (StatusCode, i32, String) {
    let (status, code) = driver_fault(&error);
    (status, code, error.message)
}

/// Map a driver error to `(status, A2A error envelope)`.
pub(super) fn error_response(err: RunError) -> Response {
    let (status, code, message) = match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) => (StatusCode::BAD_REQUEST, -32600, message),
        (RunErrorKind::Internal, message) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, message),
        (RunErrorKind::Unavailable, message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
        (RunErrorKind::Conflict, message) => (StatusCode::CONFLICT, -32603, message),
    };
    (status, Json(ErrorResponse::new(code, message))).into_response()
}

fn rest_driver_error(err: RunError) -> Response {
    let (status, code) = driver_fault(&err);
    (
        status,
        Json(json!({ "code": code, "message": err.message })),
    )
        .into_response()
}

pub(super) fn stream_binding_error(
    error: RunError,
    rpc_id: Option<&Value>,
    rest_errors: bool,
    version: ProtocolVersion,
) -> Response {
    if let Some(id) = rpc_id {
        let (code, message) = rpc_fault(error);
        rpc_error(id.clone(), code, message)
    } else if rest_errors {
        rest_driver_error_version(error, version)
    } else {
        error_response(error)
    }
}

pub(super) fn rest_driver_error_version(error: RunError, version: ProtocolVersion) -> Response {
    if version == ProtocolVersion::V03 {
        return rest_driver_error(error);
    }
    let (status, code) = driver_fault(&error);
    v1_fault(status, code, error.message)
}

pub(super) fn sse_event(
    response: &StreamResponse,
    rpc_id: Option<&Value>,
    version: ProtocolVersion,
) -> String {
    let event = match version {
        ProtocolVersion::V03 => response.event_value(),
        ProtocolVersion::V1 => v1_stream_value(response),
    };
    let payload = match (rpc_id, version) {
        (Some(id), _) => json!({ "jsonrpc": "2.0", "id": id, "result": event }),
        (None, ProtocolVersion::V03) => response.oneof_value(),
        (None, ProtocolVersion::V1) => event,
    };
    format!("data: {payload}\n\n")
}

pub(super) fn stream_response(rx: mpsc::UnboundedReceiver<String>) -> Response {
    let stream = UnboundedReceiverStream::new(rx).map(Ok::<String, Infallible>);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("valid A2A SSE response")
}

pub(crate) fn a2a_fault(status: StatusCode, code: i32, message: impl Into<String>) -> Response {
    let message = message.into();
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

pub(crate) fn v1_json_response(status: StatusCode, value: Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/a2a+json"),
    );
    response
}

pub(crate) fn v1_fault(status: StatusCode, code: i32, message: impl Into<String>) -> Response {
    let reason = match code {
        -32001 => "TASK_NOT_FOUND",
        -32002 => "TASK_NOT_CANCELABLE",
        -32003 => "PUSH_NOTIFICATION_NOT_SUPPORTED",
        -32004 => "UNSUPPORTED_OPERATION",
        -32005 => "CONTENT_TYPE_NOT_SUPPORTED",
        -32006 => "INVALID_AGENT_RESPONSE",
        -32007 => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
        -32008 => "EXTENSION_SUPPORT_REQUIRED",
        -32009 => "VERSION_NOT_SUPPORTED",
        -32603 => "INTERNAL_ERROR",
        _ => "INVALID_PARAMS",
    };
    let grpc_status = match code {
        -32001 => "NOT_FOUND",
        -32603 => "INTERNAL",
        -32009..=-32002 => "FAILED_PRECONDITION",
        _ => "INVALID_ARGUMENT",
    };
    v1_json_response(
        status,
        json!({ "error": {
            "code": status.as_u16(),
            "status": grpc_status,
            "message": message.into(),
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": reason,
                "domain": "a2a-protocol.org"
            }]
        }}),
    )
}

pub(super) fn version_fault(
    version: ProtocolVersion,
    status: StatusCode,
    code: i32,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    match version {
        ProtocolVersion::V03 => a2a_fault(status, code, message),
        ProtocolVersion::V1 => v1_fault(status, code, message),
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[tokio::test]
    async fn driver_error_facts_survive_version_and_binding_projection() {
        // Cross product: fact × protocol version. Prose deliberately does not
        // contain the old English prefixes, so translation cannot change routing.
        for version in [ProtocolVersion::V03, ProtocolVersion::V1] {
            for (kind, code, status, rpc_code) in [
                (
                    RunErrorKind::BadRequest,
                    "not_found",
                    StatusCode::NOT_FOUND,
                    -32001,
                ),
                (
                    RunErrorKind::BadRequest,
                    "unsupported_output_modes",
                    StatusCode::BAD_REQUEST,
                    -32005,
                ),
                (
                    RunErrorKind::BadRequest,
                    "invalid_request",
                    StatusCode::BAD_REQUEST,
                    -32602,
                ),
                (
                    RunErrorKind::Unavailable,
                    "provider_offline",
                    StatusCode::SERVICE_UNAVAILABLE,
                    -32603,
                ),
                (
                    RunErrorKind::Internal,
                    "internal",
                    StatusCode::INTERNAL_SERVER_ERROR,
                    -32603,
                ),
            ] {
                let make_error = || RunError {
                    kind,
                    code: code.into(),
                    message: "原因已本地化".into(),
                };
                assert_eq!(
                    rest_driver_error_version(make_error(), version).status(),
                    status
                );
                assert_eq!(rpc_fault(make_error()).0, rpc_code);
                assert_eq!(cancel_driver_error(make_error()).0, status);
            }
        }
    }

    #[test]
    fn prose_cannot_impersonate_a_classified_error() {
        assert_eq!(
            rpc_fault(RunError::bad_request(
                "unsupported output modes: ordinary prose"
            ))
            .0,
            -32602
        );
        assert_eq!(
            driver_fault(&RunError::bad_request("task not found: ordinary prose")).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            driver_fault(&RunError::unavailable_classified(
                "not_found",
                "unavailable"
            ))
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
