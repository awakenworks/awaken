//! A2A response, fault, and streaming-envelope projection.

use std::convert::Infallible;

use awaken_session_contract::{RunApplicationError, RunErrorKind};
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
pub(super) fn rpc_fault(err: RunApplicationError) -> (i32, String) {
    match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) if message.starts_with("unsupported output modes") => {
            (-32005, message)
        }
        (RunErrorKind::BadRequest, message) => (-32602, message),
        (RunErrorKind::Internal | RunErrorKind::Unavailable, message) => (-32603, message),
    }
}

pub(super) fn cancel_driver_error(error: RunApplicationError) -> (StatusCode, i32, String) {
    let status = match error.kind {
        RunErrorKind::BadRequest => StatusCode::BAD_REQUEST,
        RunErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        RunErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, -32603, error.message)
}

/// Map a driver error to `(status, A2A error envelope)`.
pub(super) fn error_response(err: RunApplicationError) -> Response {
    let (status, code, message) = match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) => (StatusCode::BAD_REQUEST, -32600, message),
        (RunErrorKind::Internal, message) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, message),
        (RunErrorKind::Unavailable, message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
    };
    (status, Json(ErrorResponse::new(code, message))).into_response()
}

fn rest_driver_error(err: RunApplicationError) -> Response {
    let (status, code, message) = match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) if message.starts_with("task not found") => {
            (StatusCode::NOT_FOUND, -32001, message)
        }
        (RunErrorKind::BadRequest, message) if message.starts_with("unsupported output modes") => {
            (StatusCode::BAD_REQUEST, -32005, message)
        }
        (RunErrorKind::BadRequest, message) => (StatusCode::BAD_REQUEST, -32602, message),
        (RunErrorKind::Internal, message) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, message),
        (RunErrorKind::Unavailable, message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
    };
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

pub(super) fn stream_binding_error(
    error: RunApplicationError,
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

pub(super) fn rest_driver_error_version(
    error: RunApplicationError,
    version: ProtocolVersion,
) -> Response {
    if version == ProtocolVersion::V03 {
        return rest_driver_error(error);
    }
    let (code, message) = rpc_fault(error);
    let status = match code {
        -32001 => StatusCode::NOT_FOUND,
        -32603 => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    v1_fault(status, code, message)
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
