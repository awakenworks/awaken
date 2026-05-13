use std::collections::BTreeMap;

use awaken_contract::contract::storage::StorageError;
use awaken_protocol_a2a::TaskState;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::types::{A2A_VERSION, SUPPORTED_OUTPUT_MODE};

#[derive(Debug)]
pub(super) enum A2aError {
    Validation {
        message: String,
        violations: Vec<FieldViolation>,
    },
    Specific {
        http_status: StatusCode,
        status: &'static str,
        reason: &'static str,
        message: String,
        metadata: BTreeMap<String, String>,
    },
    NotFound(String),
    Internal(String),
}

#[derive(Debug, Clone)]
pub(super) struct FieldViolation {
    pub(super) field: String,
    pub(super) description: String,
}

impl A2aError {
    pub(super) fn invalid(field: impl Into<String>, description: impl Into<String>) -> Self {
        Self::Validation {
            message: "invalid A2A request".to_string(),
            violations: vec![FieldViolation {
                field: field.into(),
                description: description.into(),
            }],
        }
    }

    pub(super) fn merge_invalid(
        message: impl Into<String>,
        violations: impl IntoIterator<Item = FieldViolation>,
    ) -> Self {
        Self::Validation {
            message: message.into(),
            violations: violations.into_iter().collect(),
        }
    }

    pub(super) fn version_not_supported(found: impl Into<String>) -> Self {
        let found = found.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("supportedVersion".to_string(), A2A_VERSION.to_string());
        metadata.insert("requestedVersion".to_string(), found.clone());
        Self::Specific {
            http_status: StatusCode::BAD_REQUEST,
            status: "FAILED_PRECONDITION",
            reason: "VERSION_NOT_SUPPORTED",
            message: format!("unsupported A2A-Version '{found}'"),
            metadata,
        }
    }

    pub(super) fn unsupported_operation(message: impl Into<String>) -> Self {
        Self::Specific {
            http_status: StatusCode::NOT_IMPLEMENTED,
            status: "UNIMPLEMENTED",
            reason: "UNSUPPORTED_OPERATION",
            message: message.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub(super) fn content_type_not_supported(found: impl Into<String>) -> Self {
        let found = found.into();
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "supportedOutputModes".to_string(),
            SUPPORTED_OUTPUT_MODE.to_string(),
        );
        metadata.insert("requestedOutputModes".to_string(), found);
        Self::Specific {
            http_status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            status: "INVALID_ARGUMENT",
            reason: "CONTENT_TYPE_NOT_SUPPORTED",
            message: "requested output mode is not supported".to_string(),
            metadata,
        }
    }

    pub(super) fn task_not_found(task_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        Self::Specific {
            http_status: StatusCode::NOT_FOUND,
            status: "NOT_FOUND",
            reason: "TASK_NOT_FOUND",
            message: format!("task not found: {task_id}"),
            metadata,
        }
    }

    pub(super) fn task_not_cancelable(task_id: impl Into<String>, state: TaskState) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("state".to_string(), task_state_name(state).to_string());
        Self::Specific {
            http_status: StatusCode::CONFLICT,
            status: "FAILED_PRECONDITION",
            reason: "TASK_NOT_CANCELABLE",
            message: format!("task is not cancelable in state {}", task_state_name(state)),
            metadata,
        }
    }

    pub(super) fn push_config_not_found(
        task_id: impl Into<String>,
        config_id: impl Into<String>,
    ) -> Self {
        let task_id = task_id.into();
        let config_id = config_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("configId".to_string(), config_id.clone());
        Self::Specific {
            http_status: StatusCode::NOT_FOUND,
            status: "NOT_FOUND",
            reason: "TASK_NOT_FOUND",
            message: format!("push notification config not found for task {task_id}: {config_id}"),
            metadata,
        }
    }

    pub(super) fn task_not_subscribable(task_id: impl Into<String>, state: TaskState) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("state".to_string(), task_state_name(state).to_string());
        Self::Specific {
            http_status: StatusCode::CONFLICT,
            status: "FAILED_PRECONDITION",
            reason: "UNSUPPORTED_OPERATION",
            message: format!(
                "task {task_id} is already in terminal state {}; subscribe is not available",
                task_state_name(state)
            ),
            metadata,
        }
    }

    pub(super) fn unauthenticated(message: impl Into<String>) -> Self {
        Self::Specific {
            http_status: StatusCode::UNAUTHORIZED,
            status: "UNAUTHENTICATED",
            reason: "UNAUTHENTICATED",
            message: message.into(),
            metadata: BTreeMap::new(),
        }
    }
}

impl IntoResponse for A2aError {
    fn into_response(self) -> Response {
        match self {
            Self::Validation {
                message,
                violations,
            } => (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "code": 400,
                        "status": "INVALID_ARGUMENT",
                        "message": message,
                        "details": [{
                            "@type": "type.googleapis.com/google.rpc.BadRequest",
                            "fieldViolations": violations.into_iter().map(|violation| json!({
                                "field": violation.field,
                                "description": violation.description,
                            })).collect::<Vec<_>>()
                        }]
                    }
                })),
            )
                .into_response(),
            Self::Specific {
                http_status,
                status,
                reason,
                message,
                metadata,
            } => (
                http_status,
                Json(json!({
                    "error": {
                        "code": http_status.as_u16(),
                        "status": status,
                        "message": message,
                        "details": [{
                            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                            "reason": reason,
                            "domain": "a2a-protocol.org",
                            "metadata": metadata,
                        }]
                    }
                })),
            )
                .into_response(),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": {
                        "code": 404,
                        "status": "NOT_FOUND",
                        "message": message,
                    }
                })),
            )
                .into_response(),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "code": 500,
                        "status": "INTERNAL",
                        "message": message,
                    }
                })),
            )
                .into_response(),
        }
    }
}

pub(super) fn map_a2a_storage_error(error: StorageError) -> A2aError {
    match error {
        StorageError::Validation(message) => A2aError::Validation {
            message: "invalid A2A request".to_string(),
            violations: vec![FieldViolation {
                field: "thread".to_string(),
                description: message,
            }],
        },
        other => A2aError::Internal(other.to_string()),
    }
}

pub(super) fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
        TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
        TaskState::Canceled => "TASK_STATE_CANCELED",
        TaskState::Rejected => "TASK_STATE_REJECTED",
    }
}
