//! Tool descriptor, result types, execution context, and async execution trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::event::AgentEvent;
use super::event_sink::EventSink;
use super::identity::RunIdentity;
use super::profile::AgentProfile;
use crate::state::{Snapshot, StateKey};

/// Tool execution status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Execution succeeded.
    Success,
    /// Execution succeeded with warnings.
    Warning,
    /// Execution is pending (waiting for suspension resolution).
    Pending,
    /// Execution failed.
    Error,
}

/// Result of tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Tool name.
    pub tool_name: String,
    /// Execution status.
    pub status: ToolStatus,
    /// Result data.
    pub data: Value,
    /// Optional message.
    pub message: Option<String>,
    /// Optional suspension ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension: Option<Box<crate::contract::suspension::SuspendTicket>>,
}

impl ToolResult {
    /// Create a success result.
    pub fn success(tool_name: impl Into<String>, data: impl Into<Value>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Success,
            data: data.into(),
            message: None,

            suspension: None,
        }
    }

    /// Create a success result with message.
    pub fn success_with_message(
        tool_name: impl Into<String>,
        data: impl Into<Value>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Success,
            data: data.into(),
            message: Some(message.into()),

            suspension: None,
        }
    }

    /// Create an error result.
    pub fn error(tool_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Error,
            data: Value::Null,
            message: Some(message.into()),

            suspension: None,
        }
    }

    /// Create a structured error result with stable error code payload.
    pub fn error_with_code(
        tool_name: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let code = code.into();
        let message = message.into();
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Error,
            data: serde_json::json!({
                "error": {
                    "code": code,
                    "message": message,
                }
            }),
            message: Some(format!("[{code}] {message}")),

            suspension: None,
        }
    }

    /// Create a suspended result (waiting for external resume/decision).
    pub fn suspended(tool_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Pending,
            data: Value::Null,
            message: Some(message.into()),

            suspension: None,
        }
    }

    /// Create a suspended result with a suspension ticket.
    pub fn suspended_with(
        tool_name: impl Into<String>,
        message: impl Into<String>,
        ticket: crate::contract::suspension::SuspendTicket,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Pending,
            data: Value::Null,
            message: Some(message.into()),

            suspension: Some(Box::new(ticket)),
        }
    }

    /// Create a warning result.
    pub fn warning(
        tool_name: impl Into<String>,
        data: impl Into<Value>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Warning,
            data: data.into(),
            message: Some(message.into()),

            suspension: None,
        }
    }

    /// Check if execution succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self.status, ToolStatus::Success | ToolStatus::Warning)
    }

    /// Check if execution is pending.
    pub fn is_pending(&self) -> bool {
        matches!(self.status, ToolStatus::Pending)
    }

    /// Check if execution failed.
    pub fn is_error(&self) -> bool {
        matches!(self.status, ToolStatus::Error)
    }

    /// Convert to JSON value for serialization.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Tool execution errors.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Denied: {0}")]
    Denied(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Tool descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
    pub category: Option<String>,
}

impl ToolDescriptor {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            category: None,
        }
    }

    #[must_use]
    pub fn with_parameters(mut self, schema: Value) -> Self {
        self.parameters = schema;
        self
    }

    #[must_use]
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }
}

/// Context provided to a tool during execution.
///
/// Gives the tool access to call identity, run identity, state snapshot,
/// and agent profile. All read-only — tools produce results, not state mutations.
/// Tools can optionally report activity progress via [`activity_sink`](Self::activity_sink).
#[derive(Clone)]
pub struct ToolCallContext {
    /// Unique ID of this tool call.
    pub call_id: String,
    /// Run identity (thread_id, run_id, agent_id).
    pub run_identity: RunIdentity,
    /// Active agent profile.
    pub profile: Arc<AgentProfile>,
    /// State snapshot at the time of tool execution.
    pub snapshot: Snapshot,
    /// Optional sink for reporting activity progress during execution.
    pub activity_sink: Option<Arc<dyn EventSink>>,
}

impl ToolCallContext {
    /// Read a state key from the snapshot.
    pub fn state<K: StateKey>(&self) -> Option<&K::Value> {
        self.snapshot.get::<K>()
    }

    /// Report an activity snapshot (full state replacement) for this tool call.
    ///
    /// Emits an [`AgentEvent::ActivitySnapshot`] with `message_id` set to this
    /// context's `call_id`. No-op if no `activity_sink` is configured.
    pub async fn report_activity(&self, activity_type: &str, content: &str) {
        if let Some(sink) = &self.activity_sink {
            sink.emit(AgentEvent::ActivitySnapshot {
                message_id: self.call_id.clone(),
                activity_type: activity_type.to_string(),
                content: serde_json::Value::String(content.to_string()),
                replace: Some(true),
            })
            .await;
        }
    }

    /// Report an incremental activity delta for this tool call.
    ///
    /// Emits an [`AgentEvent::ActivityDelta`] with `message_id` set to this
    /// context's `call_id`. No-op if no `activity_sink` is configured.
    pub async fn report_activity_delta(&self, activity_type: &str, patch: serde_json::Value) {
        if let Some(sink) = &self.activity_sink {
            let patches = if let serde_json::Value::Array(arr) = patch {
                arr
            } else {
                vec![patch]
            };
            sink.emit(AgentEvent::ActivityDelta {
                message_id: self.call_id.clone(),
                activity_type: activity_type.to_string(),
                patch: patches,
            })
            .await;
        }
    }

    /// Create a minimal context for testing.
    pub fn test_default() -> Self {
        Self {
            call_id: String::new(),
            run_identity: RunIdentity::default(),
            profile: Arc::new(AgentProfile::default()),
            snapshot: Snapshot::new(0, Arc::new(crate::state::StateMap::default())),
            activity_sink: None,
        }
    }
}

/// Async trait for implementing agent tools.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the descriptor for this tool.
    fn descriptor(&self) -> ToolDescriptor;

    /// Validate arguments before execution. Default: accept all.
    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    /// Execute the tool with the given arguments and context.
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_result_success() {
        let result = ToolResult::success("calc", json!(42));
        assert!(result.is_success());
        assert!(!result.is_error());
        assert!(!result.is_pending());
        assert_eq!(result.tool_name, "calc");
        assert_eq!(result.data, json!(42));
    }

    #[test]
    fn tool_result_error() {
        let result = ToolResult::error("calc", "division by zero");
        assert!(result.is_error());
        assert!(!result.is_success());
        assert_eq!(result.message.as_deref(), Some("division by zero"));
    }

    #[test]
    fn tool_result_error_with_code() {
        let result = ToolResult::error_with_code("calc", "DIV_ZERO", "division by zero");
        assert!(result.is_error());
        assert_eq!(result.data["error"]["code"], "DIV_ZERO");
        assert_eq!(
            result.message.as_deref(),
            Some("[DIV_ZERO] division by zero")
        );
    }

    #[test]
    fn tool_result_suspended() {
        let result = ToolResult::suspended("dangerous_tool", "needs approval");
        assert!(result.is_pending());
        assert!(!result.is_success());
    }

    #[test]
    fn tool_result_warning() {
        let result = ToolResult::warning("search", json!({"hits": 0}), "no results");
        assert!(result.is_success()); // warning counts as success
        assert_eq!(result.status, ToolStatus::Warning);
    }

    #[test]
    fn tool_result_serde_roundtrip() {
        let result = ToolResult::success_with_message("calc", json!(42), "done");
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "calc");
        assert_eq!(parsed.status, ToolStatus::Success);
        assert_eq!(parsed.data, json!(42));
        assert_eq!(parsed.message.as_deref(), Some("done"));
    }

    #[test]
    fn tool_descriptor_builder() {
        let desc = ToolDescriptor::new("calc", "calculator", "Math operations")
            .with_parameters(json!({"type": "object", "properties": {"expr": {"type": "string"}}}))
            .with_category("math");

        assert_eq!(desc.id, "calc");
        assert_eq!(desc.name, "calculator");
        assert_eq!(desc.category.as_deref(), Some("math"));
    }

    #[test]
    fn tool_descriptor_serde_roundtrip() {
        let desc = ToolDescriptor::new("search", "web_search", "Search the web");
        let json = serde_json::to_string(&desc).unwrap();
        let parsed: ToolDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "search");
        assert_eq!(parsed.name, "web_search");
    }

    #[test]
    fn tool_descriptor_defaults_to_empty_object_schema() {
        let desc = ToolDescriptor::new("search", "web_search", "Search the web");

        assert_eq!(desc.parameters["type"], "object");
        assert_eq!(desc.parameters["properties"], json!({}));
        assert!(desc.category.is_none());
    }

    #[test]
    fn tool_result_to_json() {
        let result = ToolResult::success("calc", json!(42));
        let value = result.to_json();
        assert_eq!(value["tool_name"], "calc");
        assert_eq!(value["status"], "success");
    }

    #[test]
    fn tool_error_display_strings_are_stable() {
        assert_eq!(
            ToolError::InvalidArguments("bad input".into()).to_string(),
            "Invalid arguments: bad input"
        );
        assert_eq!(
            ToolError::ExecutionFailed("boom".into()).to_string(),
            "Execution failed: boom"
        );
        assert_eq!(ToolError::Denied("nope".into()).to_string(), "Denied: nope");
        assert_eq!(
            ToolError::NotFound("missing".into()).to_string(),
            "Not found: missing"
        );
        assert_eq!(
            ToolError::Internal("oops".into()).to_string(),
            "Internal error: oops"
        );
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Echoes input")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("echo", args))
        }
    }

    #[tokio::test]
    async fn tool_trait_execute() {
        let tool = EchoTool;
        let result = tool
            .execute(json!({"msg": "hello"}), &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["msg"], "hello");
    }

    #[test]
    fn tool_trait_descriptor() {
        let tool = EchoTool;
        let desc = tool.descriptor();
        assert_eq!(desc.id, "echo");
    }

    #[test]
    fn tool_trait_validate_args_default_accepts() {
        let tool = EchoTool;
        assert!(tool.validate_args(&json!({})).is_ok());
    }

    struct ValidatingTool;

    #[async_trait]
    impl Tool for ValidatingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("v", "v", "validates")
        }

        fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
            if args.get("required").is_none() {
                return Err(ToolError::InvalidArguments("missing required".into()));
            }
            Ok(())
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("v", json!(null)))
        }
    }

    #[test]
    fn tool_validate_args_rejects_invalid() {
        let tool = ValidatingTool;
        assert!(tool.validate_args(&json!({})).is_err());
        assert!(tool.validate_args(&json!({"required": true})).is_ok());
    }

    #[tokio::test]
    async fn tool_as_dyn_trait_object() {
        let tool: Box<dyn Tool> = Box::new(EchoTool);
        let desc = tool.descriptor();
        assert_eq!(desc.id, "echo");
        let result = tool
            .execute(json!("test"), &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(result.is_success());
    }

    /// A mock tool that reports activity during execution.
    struct ActivityReportingTool;

    #[async_trait]
    impl Tool for ActivityReportingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("reporter", "reporter", "Reports activity")
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            ctx.report_activity("progress", "50% done").await;
            ctx.report_activity_delta(
                "progress",
                json!({"op": "replace", "path": "/percent", "value": 100}),
            )
            .await;
            Ok(ToolResult::success("reporter", json!(null)))
        }
    }

    #[tokio::test]
    async fn tool_can_report_activity_snapshot() {
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::VecEventSink;

        let sink = Arc::new(VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-1".into();
        ctx.activity_sink = Some(sink.clone() as Arc<dyn super::super::event_sink::EventSink>);

        let tool = ActivityReportingTool;
        let result = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(result.is_success());

        let events = sink.events();
        assert_eq!(events.len(), 2);

        // First event: ActivitySnapshot
        match &events[0] {
            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                assert_eq!(message_id, "call-1");
                assert_eq!(activity_type, "progress");
                assert_eq!(content, &json!("50% done"));
                assert_eq!(*replace, Some(true));
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }

        // Second event: ActivityDelta
        match &events[1] {
            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                assert_eq!(message_id, "call-1");
                assert_eq!(activity_type, "progress");
                assert_eq!(patch.len(), 1);
                assert_eq!(patch[0]["op"], "replace");
            }
            other => panic!("expected ActivityDelta, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_activity_events_include_call_id() {
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::VecEventSink;

        let sink = Arc::new(VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "unique-call-42".into();
        ctx.activity_sink = Some(sink.clone() as Arc<dyn super::super::event_sink::EventSink>);

        ctx.report_activity("status", "running").await;
        ctx.report_activity_delta(
            "status",
            json!([{"op": "add", "path": "/step", "value": 1}]),
        )
        .await;

        let events = sink.events();
        assert_eq!(events.len(), 2);

        // Both events must carry the call_id as message_id
        for event in &events {
            match event {
                AgentEvent::ActivitySnapshot { message_id, .. } => {
                    assert_eq!(message_id, "unique-call-42");
                }
                AgentEvent::ActivityDelta { message_id, .. } => {
                    assert_eq!(message_id, "unique-call-42");
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn report_activity_noop_without_sink() {
        let ctx = ToolCallContext::test_default();
        // Should not panic when no sink is configured
        ctx.report_activity("status", "test").await;
        ctx.report_activity_delta("status", json!({"op": "add"}))
            .await;
    }
}
