//! Tool descriptor, result types, execution context, and async execution trait.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::event::AgentEvent;
use super::event_sink::EventSink;
use super::identity::RunIdentity;
use super::progress::{ProgressStatus, TOOL_CALL_PROGRESS_ACTIVITY_TYPE, ToolCallProgressState};
use super::suspension::ToolCallResume;
use crate::cancellation::CancellationToken;
use crate::registry_spec::AgentSpec;
use crate::state::{Snapshot, StateCommand, StateKey};

/// Tool execution status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Execution succeeded.
    Success,
    /// Execution is pending (waiting for suspension resolution).
    ///
    /// The loop runner maps this to `ToolCallOutcome::Suspended`, which
    /// causes the sequential executor to stop and the orchestrator to
    /// transition the run to `Waiting` state until a resume decision arrives.
    Pending,
    /// Execution failed.
    ///
    /// The tool result content is sent back to the LLM as a normal tool
    /// response so it can react (retry, report, change strategy).
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
    /// Structured metadata attached by the tool executor (e.g. MCP server info,
    /// UI hints). Separate from the tool result data payload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, Value>,
}

impl ToolResult {
    /// Create a success result.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::tool::ToolResult;
    /// use serde_json::json;
    ///
    /// let result = ToolResult::success("calc", json!({"answer": 42}));
    /// assert!(result.is_success());
    /// assert!(!result.is_error());
    /// assert_eq!(result.tool_name, "calc");
    /// ```
    pub fn success(tool_name: impl Into<String>, data: impl Into<Value>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Success,
            data: data.into(),
            message: None,
            suspension: None,
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
        }
    }

    /// Create an error result.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::tool::ToolResult;
    ///
    /// let result = ToolResult::error("calc", "division by zero");
    /// assert!(result.is_error());
    /// assert!(!result.is_success());
    /// assert_eq!(result.message.as_deref(), Some("division by zero"));
    /// ```
    pub fn error(tool_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Error,
            data: Value::Null,
            message: Some(message.into()),
            suspension: None,
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
        }
    }

    /// Check if execution succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self.status, ToolStatus::Success)
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

    /// Attach a single metadata key-value pair.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// The complete output of a tool execution: result (for the LLM) + command (side-effects).
///
/// Tools that don't need side-effects can simply use `ToolResult::into()` or
/// `.into()` to create a `ToolOutput` with an empty `StateCommand`.
pub struct ToolOutput {
    /// The result data returned to the LLM.
    pub result: ToolResult,
    /// State mutations, scheduled actions, and effects produced by this tool.
    /// Uses the same `StateCommand` mechanism as plugin phase hooks.
    pub command: StateCommand,
}

impl std::fmt::Debug for ToolOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOutput")
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}

impl ToolOutput {
    /// Create a new output with the given result and an empty command.
    pub fn new(result: ToolResult) -> Self {
        Self {
            result,
            command: StateCommand::new(),
        }
    }

    /// Create a new output with both result and side-effects.
    pub fn with_command(result: ToolResult, command: StateCommand) -> Self {
        Self { result, command }
    }
}

impl From<ToolResult> for ToolOutput {
    fn from(result: ToolResult) -> Self {
        Self::new(result)
    }
}

/// Tool execution errors.
///
/// # Examples
///
/// ```
/// use awaken_contract::contract::tool::ToolError;
///
/// let err = ToolError::InvalidArguments("missing field 'x'".into());
/// assert_eq!(err.to_string(), "Invalid arguments: missing field 'x'");
///
/// let err = ToolError::NotFound("no such tool".into());
/// assert_eq!(err.to_string(), "Not found: no such tool");
/// ```
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

/// Error returned by [`TypedTool::validate`] when business-logic validation
/// rejects the deserialized arguments before execution.
#[derive(Debug, Error)]
pub enum ToolValidationError {
    /// A specific argument value was invalid.
    #[error("{message}")]
    InvalidArgument {
        /// Human-readable description of the validation failure.
        message: String,
    },
}

/// Tool descriptor.
///
/// # Examples
///
/// ```
/// use awaken_contract::contract::tool::ToolDescriptor;
/// use serde_json::json;
///
/// let desc = ToolDescriptor::new("calc", "Calculator", "Performs arithmetic")
///     .with_parameters(json!({
///         "type": "object",
///         "properties": { "expr": { "type": "string" } }
///     }));
/// assert_eq!(desc.id, "calc");
/// assert_eq!(desc.name, "Calculator");
/// assert_eq!(desc.description, "Performs arithmetic");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
    pub category: Option<String>,
    /// Arbitrary key-value metadata attached to the descriptor (e.g. MCP
    /// server info, UI hints). Not sent to the LLM.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, Value>,
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
            metadata: HashMap::new(),
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

    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Context provided to a tool during execution.
///
/// Gives the tool access to call identity, run identity, state snapshot,
/// and agent spec. All read-only — tools produce results, not state mutations.
/// Tools can optionally report activity progress via [`activity_sink`](Self::activity_sink).
#[derive(Clone)]
pub struct ToolCallContext {
    /// Unique ID of this tool call.
    pub call_id: String,
    /// Name of the tool being executed.
    pub tool_name: String,
    /// Run identity (thread_id, run_id, agent_id).
    pub run_identity: RunIdentity,
    /// Active agent spec.
    pub agent_spec: Arc<AgentSpec>,
    /// State snapshot at the time of tool execution.
    pub snapshot: Snapshot,
    /// Optional sink for reporting activity progress during execution.
    pub activity_sink: Option<Arc<dyn EventSink>>,
    /// Optional cancellation token for cooperative cancellation.
    ///
    /// Long-running tools (e.g. MCP calls, sub-agent execution) should check
    /// this token periodically via `is_cancelled()` or use `cancelled()` with
    /// `tokio::select!` to abort early when the run is cancelled.
    pub cancellation_token: Option<CancellationToken>,
    /// Resume decision input for resumed tool calls.
    pub resume_input: Option<ToolCallResume>,
    /// Active suspension id, if this execution is a resumed suspension.
    pub suspension_id: Option<String>,
    /// Active suspension reason/action, if this execution is a resumed suspension.
    pub suspension_reason: Option<String>,
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

    /// Report structured tool call progress.
    ///
    /// Emits an [`AgentEvent::ActivitySnapshot`] with
    /// `activity_type = "tool-call-progress"`. No-op if no `activity_sink` is configured.
    pub async fn report_progress(
        &self,
        status: ProgressStatus,
        message: Option<&str>,
        progress: Option<f64>,
    ) {
        if let Some(sink) = &self.activity_sink {
            let parent_call_id = self.run_identity.parent_tool_call_id.clone();
            let parent_node_id = parent_call_id
                .as_ref()
                .map(|id| format!("tool_call:{id}"))
                .or_else(|| Some(format!("run:{}", self.run_identity.run_id)));
            let state = ToolCallProgressState {
                schema: "tool-call-progress.v1".into(),
                node_id: format!("tool_call:{}", self.call_id),
                call_id: self.call_id.clone(),
                tool_name: self.tool_name.clone(),
                status,
                progress,
                loaded: None,
                total: None,
                message: message.map(ToOwned::to_owned),
                parent_node_id,
                parent_call_id,
                run_id: Some(self.run_identity.run_id.clone()),
                parent_run_id: self.run_identity.parent_run_id.clone(),
                thread_id: Some(self.run_identity.thread_id.clone()),
            };
            let content = serde_json::to_value(&state).unwrap_or_default();
            sink.emit(AgentEvent::ActivitySnapshot {
                message_id: self.call_id.clone(),
                activity_type: TOOL_CALL_PROGRESS_ACTIVITY_TYPE.into(),
                content,
                replace: Some(true),
            })
            .await;
        }
    }

    /// Emit a streaming output delta for this tool call.
    ///
    /// Sends a [`AgentEvent::ToolCallStreamDelta`] with `id` and `name` from this
    /// context, and the provided text `delta`. No-op if no `activity_sink` is configured.
    ///
    /// Use this for tools that produce incremental text output (e.g. generative UI
    /// renderers, sub-agent wrappers) that should be streamed to the frontend before
    /// the tool call completes.
    pub async fn stream_output(&self, delta: &str) {
        if let Some(sink) = &self.activity_sink {
            sink.emit(AgentEvent::ToolCallStreamDelta {
                id: self.call_id.clone(),
                name: self.tool_name.clone(),
                delta: delta.to_string(),
            })
            .await;
        }
    }

    /// Create a minimal context for testing.
    pub fn test_default() -> Self {
        Self {
            call_id: String::new(),
            tool_name: String::new(),
            run_identity: RunIdentity::default(),
            agent_spec: Arc::new(AgentSpec::default()),
            snapshot: Snapshot::new(0, Arc::new(crate::state::StateMap::default())),
            activity_sink: None,
            cancellation_token: None,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
        }
    }
}

/// A tool backed by a frontend-defined descriptor.
///
/// When the LLM calls this tool, execution suspends with
/// `UseDecisionAsToolResult` so the protocol layer can forward the call
/// to the frontend for client-side handling.
pub struct FrontEndTool {
    descriptor: ToolDescriptor,
}

impl FrontEndTool {
    pub fn new(descriptor: ToolDescriptor) -> Self {
        Self { descriptor }
    }
}

#[async_trait]
impl Tool for FrontEndTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let tool_name = &self.descriptor.id;

        if let Some(resume) = &ctx.resume_input {
            let result = match resume.result {
                Value::Bool(_) => args,
                _ => resume.result.clone(),
            };
            return Ok(ToolResult::success(tool_name, result).into());
        }

        let pending_id = if ctx.call_id.trim().is_empty() {
            tool_name.clone()
        } else {
            ctx.call_id.clone()
        };
        let ticket = crate::contract::suspension::SuspendTicket::use_decision_as_tool_result(
            crate::contract::suspension::Suspension {
                id: format!("suspend_{pending_id}"),
                action: format!("tool:{tool_name}"),
                message: format!("Frontend tool '{tool_name}' requires client execution"),
                parameters: args.clone(),
                response_schema: None,
            },
            crate::contract::suspension::PendingToolCall::new(pending_id, tool_name, args),
        );
        Ok(ToolResult::suspended_with(
            tool_name,
            format!("Tool '{tool_name}' suspended: awaiting decision"),
            ticket,
        )
        .into())
    }
}

/// Async trait for implementing agent tools.
///
/// Tools return [`ToolOutput`] which bundles both the LLM-visible result and
/// any side-effects (state mutations, scheduled actions, effects) using the
/// same [`StateCommand`] mechanism as plugin phase hooks.
///
/// Tools that don't need side-effects can return `Ok(ToolResult::success(...).into())`
/// — the `From<ToolResult>` conversion creates an empty `StateCommand` automatically.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the descriptor for this tool.
    fn descriptor(&self) -> ToolDescriptor;

    /// Validate arguments before execution. Default: accept all.
    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    /// Execute the tool with the given arguments and context.
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError>;
}

/// A strongly-typed tool trait that derives its descriptor schema from `Args`.
///
/// Implementors define an associated `Args` type that is both deserializable and
/// JSON-Schema-capable. The blanket `impl Tool for T where T: TypedTool`
/// automatically generates the [`ToolDescriptor`] (including a JSON Schema for
/// parameters) and handles argument deserialization + optional business validation
/// before dispatching to the typed [`execute`](TypedTool::execute) method.
///
/// # Example
///
/// ```ignore
/// #[derive(Deserialize, JsonSchema)]
/// struct EchoArgs { message: String }
///
/// struct EchoTool;
///
/// #[async_trait]
/// impl TypedTool for EchoTool {
///     type Args = EchoArgs;
///     fn tool_id(&self) -> &str { "echo" }
///     fn name(&self) -> &str { "echo" }
///     fn description(&self) -> &str { "Echoes back the message" }
///     async fn execute(&self, args: EchoArgs, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
///         Ok(ToolResult::success("echo", serde_json::json!({ "echo": args.message })))
///     }
/// }
/// ```
#[async_trait]
pub trait TypedTool: Send + Sync {
    /// The deserialized arguments type. Must implement `JsonSchema` for
    /// automatic schema generation and `Deserialize` for argument parsing.
    type Args: for<'de> Deserialize<'de> + schemars::JsonSchema + Send;

    /// Stable identifier used in tool-call routing.
    fn tool_id(&self) -> &str;

    /// Human-readable name shown to LLMs and UIs.
    fn name(&self) -> &str;

    /// One-line description of what the tool does.
    fn description(&self) -> &str;

    /// Optional category for grouping in registries or UIs.
    fn category(&self) -> Option<&str> {
        None
    }

    /// Optional business-logic validation run after deserialization but before
    /// execution. Return `Err(ToolValidationError)` to reject the arguments.
    fn validate(&self, _args: &Self::Args) -> Result<(), ToolValidationError> {
        Ok(())
    }

    /// Execute the tool with strongly-typed, already-validated arguments.
    async fn execute(
        &self,
        args: Self::Args,
        ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError>;
}

#[async_trait]
impl<T: TypedTool> Tool for T {
    fn descriptor(&self) -> ToolDescriptor {
        let schema = super::tool_schema::generate_tool_schema::<T::Args>();
        let mut desc = ToolDescriptor::new(self.tool_id(), self.name(), self.description())
            .with_parameters(schema);
        if let Some(cat) = self.category() {
            desc = desc.with_category(cat);
        }
        desc
    }

    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        // Normalize null/missing args to empty object so that unit-like structs
        // (e.g. `struct GetStatusArgs {}`) deserialize correctly. Some providers
        // (e.g. Gemini) pass `null` when a tool has no required parameters.
        let args = if args.is_null() {
            Value::Object(Default::default())
        } else {
            args
        };
        let typed: T::Args =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.validate(&typed)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        TypedTool::execute(self, typed, ctx).await
    }
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
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("echo", args).into())
        }
    }

    #[tokio::test]
    async fn tool_trait_execute() {
        let tool = EchoTool;
        let result = tool
            .execute(json!({"msg": "hello"}), &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(result.result.is_success());
        assert_eq!(result.result.data["msg"], "hello");
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
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("v", json!(null)).into())
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
        assert!(result.result.is_success());
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
        ) -> Result<ToolOutput, ToolError> {
            ctx.report_activity("progress", "50% done").await;
            ctx.report_activity_delta(
                "progress",
                json!({"op": "replace", "path": "/percent", "value": 100}),
            )
            .await;
            Ok(ToolResult::success("reporter", json!(null)).into())
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
        assert!(result.result.is_success());

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

    #[tokio::test]
    async fn report_progress_emits_correct_event() {
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::VecEventSink;
        use crate::contract::progress::{ProgressStatus, TOOL_CALL_PROGRESS_ACTIVITY_TYPE};

        let sink = Arc::new(VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-1".into();
        ctx.tool_name = "search".into();
        ctx.activity_sink = Some(sink.clone() as Arc<dyn EventSink>);

        ctx.report_progress(ProgressStatus::Running, Some("Searching..."), Some(0.5))
            .await;

        let events = sink.events();
        assert_eq!(events.len(), 1);

        match &events[0] {
            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                assert_eq!(message_id, "call-1");
                assert_eq!(activity_type, TOOL_CALL_PROGRESS_ACTIVITY_TYPE);
                assert_eq!(content["status"], "running");
                assert_eq!(content["tool_name"], "search");
                assert_eq!(content["progress"], 0.5);
                assert_eq!(content["message"], "Searching...");
                assert_eq!(content["node_id"], "tool_call:call-1");
                assert_eq!(content["call_id"], "call-1");
                assert_eq!(*replace, Some(true));
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn report_progress_noop_without_sink() {
        use crate::contract::progress::ProgressStatus;
        let ctx = ToolCallContext::test_default();
        ctx.report_progress(ProgressStatus::Running, None, None)
            .await;
    }

    #[tokio::test]
    async fn report_progress_populates_lineage_fields() {
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::VecEventSink;
        use crate::contract::identity::RunIdentity;
        use crate::contract::progress::ProgressStatus;

        let sink = Arc::new(VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-7".into();
        ctx.tool_name = "fetch".into();
        ctx.run_identity = RunIdentity {
            run_id: "run-abc".into(),
            parent_run_id: Some("run-parent".into()),
            thread_id: "thread-xyz".into(),
            parent_tool_call_id: Some("parent-call-1".into()),
            ..RunIdentity::default()
        };
        ctx.activity_sink = Some(sink.clone() as Arc<dyn EventSink>);

        ctx.report_progress(ProgressStatus::Running, None, None)
            .await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivitySnapshot { content, .. } => {
                assert_eq!(content["node_id"], "tool_call:call-7");
                assert_eq!(content["run_id"], "run-abc");
                assert_eq!(content["parent_run_id"], "run-parent");
                assert_eq!(content["thread_id"], "thread-xyz");
                assert_eq!(content["parent_call_id"], "parent-call-1");
                assert_eq!(content["parent_node_id"], "tool_call:parent-call-1");
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn report_progress_parent_node_id_falls_back_to_run_when_no_parent_call() {
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::VecEventSink;
        use crate::contract::identity::RunIdentity;
        use crate::contract::progress::ProgressStatus;

        let sink = Arc::new(VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-8".into();
        ctx.tool_name = "fetch".into();
        ctx.run_identity = RunIdentity {
            run_id: "run-xyz".into(),
            parent_tool_call_id: None,
            ..RunIdentity::default()
        };
        ctx.activity_sink = Some(sink.clone() as Arc<dyn EventSink>);

        ctx.report_progress(ProgressStatus::Pending, None, None)
            .await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivitySnapshot { content, .. } => {
                assert_eq!(content["parent_node_id"], "run:run-xyz");
                assert!(
                    content.get("parent_call_id").is_none() || content["parent_call_id"].is_null()
                );
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    // ── ToolStatus serde tests (migrated from uncarve) ──

    #[test]
    fn tool_status_serialization() {
        assert_eq!(
            serde_json::to_string(&ToolStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&ToolStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&ToolStatus::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn tool_status_deserialization() {
        assert_eq!(
            serde_json::from_str::<ToolStatus>("\"success\"").unwrap(),
            ToolStatus::Success
        );
        assert_eq!(
            serde_json::from_str::<ToolStatus>("\"pending\"").unwrap(),
            ToolStatus::Pending
        );
        assert_eq!(
            serde_json::from_str::<ToolStatus>("\"error\"").unwrap(),
            ToolStatus::Error
        );
    }

    #[test]
    fn tool_status_equality() {
        assert_eq!(ToolStatus::Success, ToolStatus::Success);
        assert_ne!(ToolStatus::Success, ToolStatus::Error);
    }

    #[test]
    fn tool_status_clone() {
        let status = ToolStatus::Pending;
        let cloned = status.clone();
        assert_eq!(status, cloned);
    }

    #[test]
    fn tool_status_debug() {
        assert_eq!(format!("{:?}", ToolStatus::Success), "Success");
        assert_eq!(format!("{:?}", ToolStatus::Error), "Error");
        assert_eq!(format!("{:?}", ToolStatus::Pending), "Pending");
    }

    // ── ToolResult detailed tests (migrated from uncarve) ──

    #[test]
    fn tool_result_success_detailed() {
        let result = ToolResult::success("my_tool", json!({"value": 42}));
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.data, json!({"value": 42}));
        assert!(result.message.is_none());
        assert!(result.is_success());
        assert!(!result.is_error());
        assert!(!result.is_pending());
    }

    #[test]
    fn tool_result_success_with_message_detailed() {
        let result = ToolResult::success_with_message(
            "my_tool",
            json!({"done": true}),
            "Operation complete",
        );
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.data, json!({"done": true}));
        assert_eq!(result.message, Some("Operation complete".to_string()));
        assert!(result.is_success());
    }

    #[test]
    fn tool_result_error_detailed() {
        let result = ToolResult::error("my_tool", "Something went wrong");
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(result.data, Value::Null);
        assert_eq!(result.message, Some("Something went wrong".to_string()));
        assert!(!result.is_success());
        assert!(result.is_error());
        assert!(!result.is_pending());
    }

    #[test]
    fn tool_result_error_with_code_detailed() {
        let result = ToolResult::error_with_code("my_tool", "invalid_arguments", "missing input");
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(
            result.data,
            json!({"error": {"code": "invalid_arguments", "message": "missing input"}})
        );
        assert_eq!(
            result.message,
            Some("[invalid_arguments] missing input".to_string())
        );
        assert!(result.is_error());
    }

    #[test]
    fn tool_result_pending_detailed() {
        let result = ToolResult::suspended("my_tool", "Waiting for confirmation");
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Pending);
        assert_eq!(result.data, Value::Null);
        assert_eq!(result.message, Some("Waiting for confirmation".to_string()));
        assert!(!result.is_success());
        assert!(!result.is_error());
        assert!(result.is_pending());
    }

    #[test]
    fn tool_result_with_suspension_roundtrip() {
        use crate::contract::suspension::*;
        let suspension = Suspension {
            id: "call_1".into(),
            action: "tool:confirm".into(),
            message: "Need confirmation".into(),
            parameters: json!({"message": "hi"}),
            response_schema: None,
        };
        let ticket = SuspendTicket::new(
            suspension,
            PendingToolCall::new("call_1", "confirm", json!({"message": "hi"})),
            ToolCallResumeMode::ReplayToolCall,
        );
        let result = ToolResult::suspended_with("confirm", "waiting", ticket.clone());
        assert!(result.is_pending());
        assert_eq!(*result.suspension.unwrap(), ticket);
    }

    #[test]
    fn tool_result_serialization_roundtrip() {
        let result = ToolResult::success("my_tool", json!({"key": "value"}));
        let json_str = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.tool_name, "my_tool");
        assert_eq!(parsed.status, ToolStatus::Success);
        assert_eq!(parsed.data, json!({"key": "value"}));
    }

    #[test]
    fn tool_result_clone_preserves_fields() {
        let result = ToolResult::success("my_tool", json!({"x": 1}));
        let cloned = result.clone();
        assert_eq!(result.tool_name, cloned.tool_name);
        assert_eq!(result.status, cloned.status);
        assert_eq!(result.data, cloned.data);
    }

    #[test]
    fn tool_result_debug_output() {
        let result = ToolResult::success("test", json!(null));
        let debug = format!("{:?}", result);
        assert!(debug.contains("ToolResult"));
        assert!(debug.contains("test"));
    }

    // ── ToolDescriptor detailed tests (migrated from uncarve) ──

    #[test]
    fn tool_descriptor_new_detailed() {
        let desc = ToolDescriptor::new("read_file", "Read File", "Reads a file from disk");
        assert_eq!(desc.id, "read_file");
        assert_eq!(desc.name, "Read File");
        assert_eq!(desc.description, "Reads a file from disk");
        assert!(desc.category.is_none());
        assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn tool_descriptor_with_parameters_detailed() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let desc =
            ToolDescriptor::new("read_file", "Read File", "Read").with_parameters(schema.clone());
        assert_eq!(desc.parameters, schema);
    }

    #[test]
    fn tool_descriptor_with_category_detailed() {
        let desc =
            ToolDescriptor::new("read_file", "Read File", "Read").with_category("filesystem");
        assert_eq!(desc.category, Some("filesystem".to_string()));
    }

    #[test]
    fn tool_descriptor_builder_chain_detailed() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc")
            .with_parameters(json!({"type": "object"}))
            .with_category("test");
        assert_eq!(desc.id, "tool");
        assert_eq!(desc.category, Some("test".to_string()));
    }

    #[test]
    fn tool_descriptor_serialization_detailed() {
        let desc =
            ToolDescriptor::new("my_tool", "My Tool", "Does things").with_category("utilities");
        let json_str = serde_json::to_string(&desc).unwrap();
        let parsed: ToolDescriptor = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.id, "my_tool");
        assert_eq!(parsed.name, "My Tool");
        assert_eq!(parsed.category, Some("utilities".to_string()));
    }

    #[test]
    fn tool_descriptor_clone_preserves_all() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc").with_category("cat");
        let cloned = desc.clone();
        assert_eq!(desc.id, cloned.id);
        assert_eq!(desc.name, cloned.name);
        assert_eq!(desc.description, cloned.description);
        assert_eq!(desc.category, cloned.category);
    }

    #[test]
    fn tool_descriptor_debug_output() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc");
        let debug = format!("{:?}", desc);
        assert!(debug.contains("ToolDescriptor"));
        assert!(debug.contains("tool"));
    }

    #[test]
    fn tool_result_to_json_preserves_status() {
        let result = ToolResult::error("my_tool", "fail");
        let value = result.to_json();
        assert_eq!(value["status"], "error");
        assert_eq!(value["tool_name"], "my_tool");
        assert_eq!(value["message"], "fail");
    }

    #[test]
    fn tool_result_suspended_with_null_suspension() {
        let result = ToolResult::suspended("my_tool", "waiting");
        assert!(result.suspension.is_none());
    }

    #[tokio::test]
    async fn tool_trait_arc_dyn() {
        let tool: std::sync::Arc<dyn Tool> = std::sync::Arc::new(EchoTool);
        let desc = tool.descriptor();
        assert_eq!(desc.id, "echo");
        let result = tool
            .execute(json!("test"), &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(result.result.is_success());
    }

    // ── ToolError individual variant tests (migrated from uncarve) ──

    #[test]
    fn tool_error_invalid_arguments_display() {
        let err = ToolError::InvalidArguments("missing field".to_string());
        assert_eq!(err.to_string(), "Invalid arguments: missing field");
    }

    #[test]
    fn tool_error_execution_failed_display() {
        let err = ToolError::ExecutionFailed("timeout".to_string());
        assert_eq!(err.to_string(), "Execution failed: timeout");
    }

    #[test]
    fn tool_error_denied_display() {
        let err = ToolError::Denied("no access".to_string());
        assert_eq!(err.to_string(), "Denied: no access");
    }

    #[test]
    fn tool_error_not_found_display() {
        let err = ToolError::NotFound("file.txt".to_string());
        assert_eq!(err.to_string(), "Not found: file.txt");
    }

    #[test]
    fn tool_error_internal_display() {
        let err = ToolError::Internal("unexpected".to_string());
        assert_eq!(err.to_string(), "Internal error: unexpected");
    }

    // ── Activity event tests ──

    fn make_ctx_with_sink() -> (
        ToolCallContext,
        Arc<crate::contract::event_sink::VecEventSink>,
    ) {
        let sink = Arc::new(crate::contract::event_sink::VecEventSink::new());
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-100".into();
        ctx.tool_name = "test_tool".into();
        ctx.activity_sink = Some(sink.clone() as Arc<dyn EventSink>);
        (ctx, sink)
    }

    #[tokio::test]
    async fn activity_snapshot_contains_content_and_type() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity("file-write", "writing to disk").await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivitySnapshot {
                activity_type,
                content,
                ..
            } => {
                assert_eq!(activity_type, "file-write");
                assert_eq!(content, &json!("writing to disk"));
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn activity_delta_contains_patch_array() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity_delta(
            "edit",
            json!([
                {"op": "replace", "path": "/line", "value": 42},
                {"op": "add", "path": "/col", "value": 0}
            ]),
        )
        .await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivityDelta { patch, .. } => {
                assert_eq!(patch.len(), 2);
                assert_eq!(patch[0]["op"], "replace");
                assert_eq!(patch[1]["op"], "add");
            }
            other => panic!("expected ActivityDelta, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn activity_snapshot_replace_true() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity("status", "done").await;

        let events = sink.events();
        match &events[0] {
            AgentEvent::ActivitySnapshot { replace, .. } => {
                assert_eq!(*replace, Some(true));
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn activity_snapshot_replace_false() {
        let (ctx, sink) = make_ctx_with_sink();
        // Emit directly via sink to test replace=Some(false)
        if let Some(sink_ref) = &ctx.activity_sink {
            sink_ref
                .emit(AgentEvent::ActivitySnapshot {
                    message_id: ctx.call_id.clone(),
                    activity_type: "log".into(),
                    content: json!("append me"),
                    replace: Some(false),
                })
                .await;
        }

        let events = sink.events();
        match &events[0] {
            AgentEvent::ActivitySnapshot { replace, .. } => {
                assert_eq!(*replace, Some(false));
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn activity_snapshot_replace_none_default() {
        let (ctx, sink) = make_ctx_with_sink();
        // Emit directly via sink to test replace=None default
        if let Some(sink_ref) = &ctx.activity_sink {
            sink_ref
                .emit(AgentEvent::ActivitySnapshot {
                    message_id: ctx.call_id.clone(),
                    activity_type: "info".into(),
                    content: json!("data"),
                    replace: None,
                })
                .await;
        }

        let events = sink.events();
        match &events[0] {
            AgentEvent::ActivitySnapshot { replace, .. } => {
                assert!(replace.is_none());
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_activities_same_call() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity("status", "starting").await;
        ctx.report_activity("status", "in progress").await;
        ctx.report_activity("status", "done").await;

        let events = sink.events();
        assert_eq!(events.len(), 3);
        for event in &events {
            match event {
                AgentEvent::ActivitySnapshot { message_id, .. } => {
                    assert_eq!(message_id, "call-100");
                }
                other => panic!("expected ActivitySnapshot, got: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn activity_type_preserved_across_snapshot_and_delta() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity("compile", "compiling").await;
        ctx.report_activity_delta(
            "compile",
            json!({"op": "replace", "path": "/step", "value": 2}),
        )
        .await;

        let events = sink.events();
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::ActivitySnapshot { activity_type, .. } => {
                assert_eq!(activity_type, "compile");
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
        match &events[1] {
            AgentEvent::ActivityDelta { activity_type, .. } => {
                assert_eq!(activity_type, "compile");
            }
            other => panic!("expected ActivityDelta, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn progress_report_has_structured_content() {
        use crate::contract::progress::ProgressStatus;

        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_progress(ProgressStatus::Running, Some("Indexing files"), Some(0.75))
            .await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivitySnapshot { content, .. } => {
                assert_eq!(content["status"], "running");
                assert_eq!(content["message"], "Indexing files");
                assert_eq!(content["progress"], 0.75);
                assert_eq!(content["tool_name"], "test_tool");
                assert_eq!(content["call_id"], "call-100");
                assert_eq!(content["node_id"], "tool_call:call-100");
                assert_eq!(content["schema"], "tool-call-progress.v1");
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn progress_report_activity_type_is_tool_call_progress() {
        use crate::contract::progress::{ProgressStatus, TOOL_CALL_PROGRESS_ACTIVITY_TYPE};

        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_progress(ProgressStatus::Done, None, None).await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ActivitySnapshot { activity_type, .. } => {
                assert_eq!(activity_type, TOOL_CALL_PROGRESS_ACTIVITY_TYPE);
            }
            other => panic!("expected ActivitySnapshot, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn activity_events_order_preserved() {
        let (ctx, sink) = make_ctx_with_sink();
        ctx.report_activity("step", "one").await;
        ctx.report_activity_delta("step", json!({"op": "add", "path": "/n", "value": 2}))
            .await;
        ctx.report_activity("step", "three").await;

        let events = sink.events();
        assert_eq!(events.len(), 3);

        // First: snapshot "one"
        assert!(
            matches!(&events[0], AgentEvent::ActivitySnapshot { content, .. } if content == &json!("one"))
        );
        // Second: delta
        assert!(matches!(&events[1], AgentEvent::ActivityDelta { .. }));
        // Third: snapshot "three"
        assert!(
            matches!(&events[2], AgentEvent::ActivitySnapshot { content, .. } if content == &json!("three"))
        );
    }

    mod typed_tool_tests {
        use super::super::super::tool_schema::validate_against_schema;
        use super::super::*;
        use schemars::JsonSchema;
        use serde::Deserialize;
        use serde_json::json;

        // ── Test structs and tools ──

        #[derive(Deserialize, JsonSchema)]
        struct EchoArgs {
            message: String,
        }

        struct TypedEchoTool;

        #[async_trait]
        impl TypedTool for TypedEchoTool {
            type Args = EchoArgs;
            fn tool_id(&self) -> &str {
                "echo"
            }
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "Echoes back the message"
            }
            async fn execute(
                &self,
                args: EchoArgs,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolResult::success("echo", json!({ "echo": args.message })).into())
            }
        }

        #[derive(Deserialize, JsonSchema)]
        struct StrictArgs {
            name: String,
        }

        struct StrictTool;

        #[async_trait]
        impl TypedTool for StrictTool {
            type Args = StrictArgs;
            fn tool_id(&self) -> &str {
                "strict"
            }
            fn name(&self) -> &str {
                "strict"
            }
            fn description(&self) -> &str {
                "Rejects empty names"
            }
            fn validate(&self, args: &StrictArgs) -> Result<(), ToolValidationError> {
                if args.name.is_empty() {
                    Err(ToolValidationError::InvalidArgument {
                        message: "name must not be empty".into(),
                    })
                } else {
                    Ok(())
                }
            }
            async fn execute(
                &self,
                args: StrictArgs,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolResult::success("strict", json!({ "name": args.name })).into())
            }
        }

        struct CategorizedTool;

        #[async_trait]
        impl TypedTool for CategorizedTool {
            type Args = EchoArgs;
            fn tool_id(&self) -> &str {
                "categorized"
            }
            fn name(&self) -> &str {
                "categorized"
            }
            fn description(&self) -> &str {
                "Has a category"
            }
            fn category(&self) -> Option<&str> {
                Some("utility")
            }
            async fn execute(
                &self,
                args: EchoArgs,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolResult::success("categorized", json!({ "echo": args.message })).into())
            }
        }

        // ── Tests ──

        #[test]
        fn typed_tool_descriptor_has_schema() {
            let tool = TypedEchoTool;
            let desc = tool.descriptor();
            assert_eq!(desc.id, "echo");
            assert_eq!(desc.name, "echo");
            assert_eq!(desc.parameters["type"], "object");
            assert!(desc.parameters["properties"]["message"].is_object());
        }

        #[test]
        fn typed_tool_descriptor_schema_is_valid() {
            let tool = TypedEchoTool;
            let desc = tool.descriptor();
            assert!(jsonschema::validator_for(&desc.parameters).is_ok());
        }

        #[tokio::test]
        async fn typed_tool_execute_valid_args() {
            let tool = TypedEchoTool;
            let result = Tool::execute(
                &tool,
                json!({"message": "hello"}),
                &ToolCallContext::test_default(),
            )
            .await
            .unwrap();
            assert!(result.result.is_success());
            assert_eq!(result.result.data["echo"], "hello");
        }

        #[tokio::test]
        async fn typed_tool_rejects_wrong_type() {
            let tool = TypedEchoTool;
            let err = Tool::execute(
                &tool,
                json!({"message": 123}),
                &ToolCallContext::test_default(),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)));
        }

        #[tokio::test]
        async fn typed_tool_rejects_missing_required() {
            let tool = TypedEchoTool;
            let err = Tool::execute(&tool, json!({}), &ToolCallContext::test_default())
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)));
        }

        #[tokio::test]
        async fn typed_tool_business_validation_rejects() {
            let tool = StrictTool;
            let err = Tool::execute(&tool, json!({"name": ""}), &ToolCallContext::test_default())
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)));
        }

        #[tokio::test]
        async fn typed_tool_business_validation_passes() {
            let tool = StrictTool;
            let result = Tool::execute(
                &tool,
                json!({"name": "Alice"}),
                &ToolCallContext::test_default(),
            )
            .await
            .unwrap();
            assert!(result.result.is_success());
            assert_eq!(result.result.data["name"], "Alice");
        }

        #[test]
        fn typed_tool_category_propagated() {
            let tool = CategorizedTool;
            let desc = tool.descriptor();
            assert_eq!(desc.category.as_deref(), Some("utility"));
        }

        #[test]
        fn typed_tool_schema_validates_correct_input() {
            let tool = TypedEchoTool;
            let desc = tool.descriptor();
            let args = json!({"message": "hello"});
            assert!(validate_against_schema(&desc.parameters, &args).is_ok());
        }

        #[test]
        fn typed_tool_schema_rejects_incorrect_input() {
            let tool = TypedEchoTool;
            let desc = tool.descriptor();
            let args = json!({"message": 123});
            assert!(validate_against_schema(&desc.parameters, &args).is_err());
        }

        // ── Null / empty args normalization (Gemini compat) ──

        #[derive(Deserialize, JsonSchema)]
        struct EmptyArgs {}

        struct NoArgsTool;

        #[async_trait]
        impl TypedTool for NoArgsTool {
            type Args = EmptyArgs;
            fn tool_id(&self) -> &str {
                "no_args"
            }
            fn name(&self) -> &str {
                "no_args"
            }
            fn description(&self) -> &str {
                "A tool with no required args"
            }
            async fn execute(
                &self,
                _args: EmptyArgs,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolResult::success("no_args", json!({"status": "ok"})).into())
            }
        }

        #[tokio::test]
        async fn typed_tool_null_args_deserialized_as_empty_object() {
            let tool = NoArgsTool;
            let ctx = ToolCallContext::test_default();
            let result = Tool::execute(&tool, Value::Null, &ctx).await;
            assert!(
                result.is_ok(),
                "null args should deserialize to EmptyArgs {{}}"
            );
        }

        #[tokio::test]
        async fn typed_tool_empty_object_args_works() {
            let tool = NoArgsTool;
            let ctx = ToolCallContext::test_default();
            let result = Tool::execute(&tool, json!({}), &ctx).await;
            assert!(
                result.is_ok(),
                "empty object args should deserialize to EmptyArgs {{}}"
            );
        }

        #[tokio::test]
        async fn typed_tool_null_args_does_not_break_required_fields() {
            let tool = TypedEchoTool;
            let ctx = ToolCallContext::test_default();
            let result = Tool::execute(&tool, Value::Null, &ctx).await;
            assert!(
                result.is_err(),
                "null args should fail for tools with required fields"
            );
        }
    }

    mod stream_output_tests {
        use super::*;
        use crate::contract::event::AgentEvent;
        use crate::contract::event_sink::EventSink;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        struct CollectSink(Mutex<Vec<AgentEvent>>);

        #[async_trait::async_trait]
        impl EventSink for CollectSink {
            async fn emit(&self, event: AgentEvent) {
                self.0.lock().await.push(event);
            }
        }

        #[tokio::test]
        async fn stream_output_emits_tool_call_stream_delta() {
            let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
            let ctx = ToolCallContext {
                call_id: "c1".into(),
                tool_name: "json_render".into(),
                activity_sink: Some(sink.clone()),
                ..ToolCallContext::test_default()
            };

            ctx.stream_output("hello ").await;
            ctx.stream_output("world").await;

            let events = sink.0.lock().await;
            assert_eq!(events.len(), 2);
            assert!(matches!(&events[0], AgentEvent::ToolCallStreamDelta {
                id, name, delta
            } if id == "c1" && name == "json_render" && delta == "hello "));
            assert!(matches!(&events[1], AgentEvent::ToolCallStreamDelta {
                id, name, delta
            } if id == "c1" && name == "json_render" && delta == "world"));
        }

        #[tokio::test]
        async fn stream_output_noop_without_sink() {
            let ctx = ToolCallContext {
                call_id: "c1".into(),
                tool_name: "render".into(),
                activity_sink: None,
                ..ToolCallContext::test_default()
            };
            // Should not panic
            ctx.stream_output("data").await;
        }
    }

    // ── FrontEndTool tests ──

    #[tokio::test]
    async fn frontend_tool_execute_returns_pending_status() {
        let desc = ToolDescriptor::new("ui_confirm", "UI Confirm", "Confirm dialog");
        let tool = FrontEndTool::new(desc);
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-fe-1".into();
        let args = json!({"prompt": "ok?"});
        let output = tool.execute(args.clone(), &ctx).await.unwrap();
        assert!(output.result.is_pending());
        let ticket = output
            .result
            .suspension
            .as_deref()
            .expect("frontend tool should attach a suspension ticket");
        assert_eq!(ticket.pending.id, "call-fe-1");
        assert_eq!(ticket.pending.name, "ui_confirm");
        assert_eq!(ticket.pending.arguments, args);
        assert_eq!(
            ticket.resume_mode,
            crate::contract::suspension::ToolCallResumeMode::UseDecisionAsToolResult
        );
    }

    #[tokio::test]
    async fn frontend_tool_uses_args_as_pending_parameters() {
        let desc = ToolDescriptor::new("ui_form", "UI Form", "Form input");
        let tool = FrontEndTool::new(desc);
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-fe-2".into();
        let args = json!({"field": "value", "count": 42});
        let output = tool.execute(args.clone(), &ctx).await.unwrap();
        assert!(output.result.is_pending());
        let ticket = output.result.suspension.as_deref().unwrap();
        assert_eq!(ticket.suspension.parameters, args);
    }

    #[tokio::test]
    async fn frontend_tool_command_is_empty() {
        let desc = ToolDescriptor::new("ui_confirm", "UI Confirm", "Confirm dialog");
        let tool = FrontEndTool::new(desc);
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-fe-4".into();
        let output = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(
            output.command.is_empty(),
            "FrontEndTool should not produce side-effects"
        );
    }

    #[test]
    fn frontend_tool_descriptor_matches_provided() {
        let desc = ToolDescriptor::new("custom_ui", "Custom UI", "Custom frontend tool")
            .with_parameters(json!({"type": "object", "properties": {"x": {"type": "string"}}}))
            .with_category("ui");
        let tool = FrontEndTool::new(desc.clone());
        let got = tool.descriptor();
        assert_eq!(got.id, "custom_ui");
        assert_eq!(got.name, "Custom UI");
        assert_eq!(got.description, "Custom frontend tool");
        assert_eq!(got.parameters, desc.parameters);
        assert_eq!(got.category, Some("ui".to_string()));
    }

    #[tokio::test]
    async fn frontend_tool_with_empty_args_returns_pending_ticket() {
        let desc = ToolDescriptor::new("ui_noop", "UI Noop", "No-op frontend tool");
        let tool = FrontEndTool::new(desc);
        let mut ctx = ToolCallContext::test_default();
        ctx.call_id = "call-fe-5".into();
        let output = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(output.result.is_pending());
        assert_eq!(
            output
                .result
                .suspension
                .as_deref()
                .unwrap()
                .pending
                .arguments,
            json!({})
        );
    }

    mod tool_validation_error_tests {
        use super::*;

        #[test]
        fn invalid_argument_formats_message() {
            let err = ToolValidationError::InvalidArgument {
                message: "name must not be empty".into(),
            };
            assert_eq!(err.to_string(), "name must not be empty");
        }

        #[test]
        fn invalid_argument_is_debug() {
            let err = ToolValidationError::InvalidArgument {
                message: "bad".into(),
            };
            let debug = format!("{err:?}");
            assert!(debug.contains("InvalidArgument"));
        }
    }

    mod cancellation_token_tests {
        use super::*;
        use crate::cancellation::CancellationToken;

        #[test]
        fn test_default_has_no_cancellation_token() {
            let ctx = ToolCallContext::test_default();
            assert!(ctx.cancellation_token.is_none());
        }

        #[test]
        fn cancellation_token_is_accessible_on_context() {
            let token = CancellationToken::new();
            let mut ctx = ToolCallContext::test_default();
            ctx.cancellation_token = Some(token.clone());

            assert!(!ctx.cancellation_token.as_ref().unwrap().is_cancelled());
            token.cancel();
            assert!(ctx.cancellation_token.as_ref().unwrap().is_cancelled());
        }

        #[test]
        fn cancellation_token_survives_clone() {
            let token = CancellationToken::new();
            let mut ctx = ToolCallContext::test_default();
            ctx.cancellation_token = Some(token.clone());

            let cloned_ctx = ctx.clone();
            token.cancel();

            assert!(
                cloned_ctx
                    .cancellation_token
                    .as_ref()
                    .unwrap()
                    .is_cancelled()
            );
        }

        /// A tool that checks the cancellation token before doing work.
        struct CancellationAwareTool;

        #[async_trait]
        impl Tool for CancellationAwareTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("cancellable", "Cancellable", "Checks cancellation")
            }

            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                if let Some(token) = &ctx.cancellation_token
                    && token.is_cancelled()
                {
                    return Ok(ToolResult::error("cancellable", "cancelled").into());
                }
                Ok(ToolResult::success("cancellable", json!("done")).into())
            }
        }

        #[tokio::test]
        async fn tool_executes_normally_without_cancellation() {
            let token = CancellationToken::new();
            let mut ctx = ToolCallContext::test_default();
            ctx.cancellation_token = Some(token);

            let tool = CancellationAwareTool;
            let output = tool.execute(json!({}), &ctx).await.unwrap();
            assert!(output.result.is_success());
        }

        #[tokio::test]
        async fn tool_detects_cancellation_via_is_cancelled() {
            let token = CancellationToken::new();
            token.cancel();
            let mut ctx = ToolCallContext::test_default();
            ctx.cancellation_token = Some(token);

            let tool = CancellationAwareTool;
            let output = tool.execute(json!({}), &ctx).await.unwrap();
            assert!(!output.result.is_success());
            assert_eq!(output.result.message.as_deref(), Some("cancelled"));
        }

        #[tokio::test]
        async fn tool_without_token_executes_normally() {
            let ctx = ToolCallContext::test_default();
            let tool = CancellationAwareTool;
            let output = tool.execute(json!({}), &ctx).await.unwrap();
            assert!(output.result.is_success());
        }

        #[tokio::test]
        async fn tool_can_select_on_cancelled_future() {
            let token = CancellationToken::new();
            let mut ctx = ToolCallContext::test_default();
            ctx.cancellation_token = Some(token.clone());

            // Spawn a task that cancels after a short delay
            let cancel_token = token.clone();
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                cancel_token.cancel();
            });

            // Simulate a tool using tokio::select! with the token
            let ct = ctx.cancellation_token.as_ref().unwrap();
            let was_cancelled = tokio::select! {
                _ = ct.cancelled() => true,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => false,
            };
            assert!(was_cancelled);
        }
    }
}
