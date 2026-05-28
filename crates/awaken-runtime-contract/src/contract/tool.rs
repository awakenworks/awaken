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
    /// use awaken_runtime_contract::contract::tool::ToolResult;
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
    /// use awaken_runtime_contract::contract::tool::ToolResult;
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
/// use awaken_runtime_contract::contract::tool::ToolError;
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
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Cancelled: {0}")]
    Cancelled(String),
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
/// use awaken_runtime_contract::contract::tool::ToolDescriptor;
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
            return Ok(ToolResult::success(tool_name, resume.result.clone()).into());
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
mod tests;
