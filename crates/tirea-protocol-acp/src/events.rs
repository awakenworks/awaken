use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{PermissionOption, StopReason, ToolCallStatus};

/// ACP protocol events emitted by the encoder.
///
/// These map to JSON-RPC 2.0 notification methods in the ACP specification:
/// - `session/update` — incremental updates during agent execution
/// - `session/request_permission` — permission prompts for tool approval
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum AcpEvent {
    /// `session/update` — carries incremental session content.
    #[serde(rename = "session/update")]
    SessionUpdate(Box<SessionUpdateParams>),

    /// `session/request_permission` — requests user approval for a tool call.
    #[serde(rename = "session/request_permission")]
    RequestPermission(RequestPermissionParams),
}

/// Payload for `session/update` notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    /// Incremental agent message text chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_message_chunk: Option<String>,

    /// Incremental agent thought/reasoning chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_thought_chunk: Option<String>,

    /// A complete tool call ready for execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<AcpToolCall>,

    /// An update to a previously emitted tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_update: Option<AcpToolCallUpdate>,

    /// Session finished indicator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<AcpFinished>,

    /// Error during session execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpError>,

    /// Full state snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_snapshot: Option<Value>,

    /// Incremental state delta (JSON Patch ops).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_delta: Option<Vec<Value>>,

    /// Activity snapshot (progress indicator for long-running operations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<AcpActivity>,
}

/// A tool call ready for execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool being called.
    pub name: String,
    /// Complete tool arguments as JSON.
    pub arguments: Value,
}

/// An update to a previously emitted tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpToolCallUpdate {
    /// Tool call identifier matching the original `tool_call.id`.
    pub id: String,
    /// Updated status.
    pub status: ToolCallStatus,
    /// Tool execution result (present when `status` is `completed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message (present when `status` is `errored`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Session finished metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpFinished {
    /// Reason the session stopped.
    pub stop_reason: StopReason,
}

/// Error information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpError {
    /// Human-readable error message.
    pub message: String,
    /// Optional error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Activity update for long-running operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpActivity {
    /// Identifier of the message/context this activity belongs to.
    pub message_id: String,
    /// Activity kind (e.g. `"tool_call_progress"`, `"thinking"`).
    pub activity_type: String,
    /// Activity payload.
    pub content: Value,
    /// When `true`, replaces the previous activity with the same `message_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
    /// JSON Patch ops for incremental activity updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Vec<Value>>,
}

/// Payload for `session/request_permission` notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    /// The tool call ID requiring permission.
    pub tool_call_id: String,
    /// Name of the tool requiring approval.
    pub tool_name: String,
    /// Tool arguments.
    pub tool_args: Value,
    /// Available permission options for the user.
    pub options: Vec<PermissionOption>,
}

// ==========================================================================
// Factory methods
// ==========================================================================

impl AcpEvent {
    /// Create a `session/update` with an agent message chunk.
    pub fn agent_message(chunk: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            agent_message_chunk: Some(chunk.into()),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a thought chunk.
    pub fn agent_thought(chunk: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            agent_thought_chunk: Some(chunk.into()),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a complete tool call.
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call: Some(AcpToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a tool call status update.
    pub fn tool_call_completed(id: impl Into<String>, result: Value) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(AcpToolCallUpdate {
                id: id.into(),
                status: ToolCallStatus::Completed,
                result: Some(result),
                error: None,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a tool call denial.
    pub fn tool_call_denied(id: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(AcpToolCallUpdate {
                id: id.into(),
                status: ToolCallStatus::Denied,
                result: None,
                error: None,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a tool call error.
    pub fn tool_call_errored(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(AcpToolCallUpdate {
                id: id.into(),
                status: ToolCallStatus::Errored,
                result: None,
                error: Some(error.into()),
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with session finished.
    pub fn finished(stop_reason: StopReason) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            finished: Some(AcpFinished { stop_reason }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with an error.
    pub fn error(message: impl Into<String>, code: Option<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            error: Some(AcpError {
                message: message.into(),
                code,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a state snapshot.
    pub fn state_snapshot(snapshot: Value) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            state_snapshot: Some(snapshot),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with a state delta.
    pub fn state_delta(delta: Vec<Value>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            state_delta: Some(delta),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with an activity snapshot.
    pub fn activity_snapshot(
        message_id: impl Into<String>,
        activity_type: impl Into<String>,
        content: Value,
        replace: Option<bool>,
    ) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            activity: Some(AcpActivity {
                message_id: message_id.into(),
                activity_type: activity_type.into(),
                content,
                replace,
                patch: None,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/update` with an activity delta.
    pub fn activity_delta(
        message_id: impl Into<String>,
        activity_type: impl Into<String>,
        patch: Vec<Value>,
    ) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            activity: Some(AcpActivity {
                message_id: message_id.into(),
                activity_type: activity_type.into(),
                content: Value::Null,
                replace: None,
                patch: Some(patch),
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    /// Create a `session/request_permission` event.
    pub fn request_permission(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_args: Value,
    ) -> Self {
        Self::RequestPermission(RequestPermissionParams {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            tool_args,
            options: vec![
                PermissionOption::AllowOnce,
                PermissionOption::AllowAlways,
                PermissionOption::RejectOnce,
                PermissionOption::RejectAlways,
            ],
        })
    }
}

impl SessionUpdateParams {
    /// Create an empty params struct (all fields `None`).
    pub fn empty() -> Self {
        Self {
            agent_message_chunk: None,
            agent_thought_chunk: None,
            tool_call: None,
            tool_call_update: None,
            finished: None,
            error: None,
            state_snapshot: None,
            state_delta: None,
            activity: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_update_roundtrip() {
        let event = AcpEvent::agent_message("hello");
        let json = serde_json::to_string(&event).unwrap();
        let restored: AcpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, restored);
    }

    #[test]
    fn request_permission_roundtrip() {
        let event = AcpEvent::request_permission("fc_1", "bash", json!({"command": "rm -rf /tmp"}));
        let json = serde_json::to_string(&event).unwrap();
        let restored: AcpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, restored);
    }

    #[test]
    fn finished_serializes_stop_reason() {
        let event = AcpEvent::finished(StopReason::EndTurn);
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["params"]["finished"]["stopReason"], "end_turn");
    }

    #[test]
    fn tool_call_update_denied_omits_result_and_error() {
        let event = AcpEvent::tool_call_denied("call_1");
        let value = serde_json::to_value(&event).unwrap();
        let update = &value["params"]["toolCallUpdate"];
        assert_eq!(update["status"], "denied");
        assert!(update.get("result").is_none() || update["result"].is_null());
        assert!(update.get("error").is_none() || update["error"].is_null());
    }

    #[test]
    fn empty_session_update_omits_all_none_fields() {
        let event = AcpEvent::SessionUpdate(Box::new(SessionUpdateParams::empty()));
        let value = serde_json::to_value(&event).unwrap();
        let params = &value["params"];
        assert!(
            params.as_object().unwrap().is_empty()
                || params.as_object().unwrap().values().all(Value::is_null)
        );
    }
}
