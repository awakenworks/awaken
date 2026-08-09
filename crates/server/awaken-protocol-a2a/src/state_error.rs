//! One A2A wire mapping for projection/config/delivery state failures.

use awaken_session_contract::{RunApplicationError, RunError};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use serde_json::json;

use crate::router::{a2a_fault, v1_fault};
use crate::state::StateError;
use crate::types::{Message, StreamResponse, TaskState, TaskStatus, TaskStatusUpdateEvent};
use crate::version::ProtocolVersion;

pub(crate) fn state_run_error(error: StateError) -> RunApplicationError {
    match error {
        StateError::Invalid(message) | StateError::NotFound(message) => {
            RunError::bad_request(message)
        }
        StateError::Storage(message) => RunError::unavailable(message),
    }
}

pub(crate) fn state_fault_response(error: StateError, version: ProtocolVersion) -> Response {
    let (status, code, message) = classify(error);
    if version == ProtocolVersion::V1 {
        v1_fault(status, code, message)
    } else {
        a2a_fault(status, code, message)
    }
}

pub(crate) fn state_rpc_response(id: Value, error: StateError) -> Response {
    let (_, code, message) = classify(error);
    rpc_error(id, code, message)
}

/// A JSON-RPC error member on a 200 (transport success, call failure).
pub(crate) fn rpc_error(id: Value, code: i32, message: impl Into<String>) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    }))
    .into_response()
}

pub(crate) fn state_cancel_error(error: StateError) -> (StatusCode, i32, String) {
    classify(error)
}

pub(crate) fn state_failure_update(
    task_id: &str,
    context_id: &str,
    error: StateError,
) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        kind: crate::types::TaskStatusUpdateKind::StatusUpdate,
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Failed,
            message: Some(Message::agent_text(
                format!("a2a-state-{task_id}"),
                error.to_string(),
            )),
            timestamp: Some(crate::time::now_rfc3339()),
        },
        final_: true,
        metadata: None,
    })
}

fn classify(error: StateError) -> (StatusCode, i32, String) {
    match error {
        StateError::Invalid(message) => (StatusCode::BAD_REQUEST, -32602, message),
        StateError::NotFound(message) => (StatusCode::NOT_FOUND, -32001, message),
        StateError::Storage(message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
    }
}
