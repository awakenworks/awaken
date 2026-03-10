//! Tool trait for agent actions.
//!
//! Tools execute actions and can modify state through `Thread`.

use super::ToolCallContext;
use crate::runtime::action::Action;
use crate::runtime::phase::SuspendTicket;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use thiserror::Error;

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
    /// Metadata.
    pub metadata: HashMap<String, Value>,
    /// Structured suspension payload for loop-level suspension handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension: Option<Box<SuspendTicket>>,
}

impl ToolResult {
    /// Create a success result.
    pub fn success(tool_name: impl Into<String>, data: impl Into<Value>) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Success,
            data: data.into(),
            message: None,
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
            suspension: None,
        }
    }

    /// Create a structured error result with stable error code payload.
    pub fn error_with_code(
        tool_name: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let tool_name = tool_name.into();
        let code = code.into();
        let message = message.into();
        Self {
            tool_name,
            status: ToolStatus::Error,
            data: serde_json::json!({
                "error": {
                    "code": code,
                    "message": message,
                }
            }),
            message: Some(format!("[{code}] {message}")),
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
            suspension: None,
        }
    }

    /// Create a suspended result carrying an explicit suspension envelope.
    pub fn suspended_with(
        tool_name: impl Into<String>,
        message: impl Into<String>,
        ticket: SuspendTicket,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            status: ToolStatus::Pending,
            data: Value::Null,
            message: Some(message.into()),
            metadata: HashMap::new(),
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
            metadata: HashMap::new(),
            suspension: None,
        }
    }

    /// Add metadata.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Attach structured suspension payload for loop-level suspension handling.
    pub fn with_suspension(mut self, ticket: SuspendTicket) -> Self {
        self.suspension = Some(Box::new(ticket));
        self
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

    /// Structured suspension payload attached by `with_suspension`.
    pub fn suspension(&self) -> Option<SuspendTicket> {
        self.suspension.as_deref().cloned()
    }

    /// Convert to JSON value for serialization.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Structured tool effect used by the action/reducer pipeline.
///
/// Tools return a [`ToolResult`] plus actions applied during `AfterToolExecute`
/// before plugin hooks run. All side effects—state changes, user messages, and
/// other context mutations—are expressed as `Box<dyn Action>`.
///
/// State actions (`AnyStateAction`) implement `Action` via `into_state_action`,
/// which allows the loop to extract them for execution-patch reduction (required
/// for parallel conflict detection) while keeping a single unified interface.
pub struct ToolExecutionEffect {
    pub result: ToolResult,
    /// All tool-emitted actions applied during `AfterToolExecute`.
    actions: Vec<Box<dyn Action>>,
}

impl ToolExecutionEffect {
    #[must_use]
    pub fn new(result: ToolResult) -> Self {
        Self {
            result,
            actions: Vec::new(),
        }
    }

    /// Add an action applied during `AfterToolExecute` before plugin hooks.
    ///
    /// Accepts any `Action` implementor, including `AnyStateAction` (state
    /// changes), `AddUserMessage` (user-facing messages), and custom actions.
    /// Only `AfterToolExecute`-compatible actions are accepted; others will be
    /// rejected at runtime by phase validation.
    #[must_use]
    pub fn with_action<A: Action + 'static>(mut self, action: A) -> Self {
        self.actions.push(Box::new(action));
        self
    }

    pub fn into_parts(self) -> (ToolResult, Vec<Box<dyn Action>>) {
        (self.result, self.actions)
    }
}

impl From<ToolResult> for ToolExecutionEffect {
    fn from(result: ToolResult) -> Self {
        Self::new(result)
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

/// Tool descriptor containing metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Unique tool ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema for parameters.
    pub parameters: Value,
    /// Tool category.
    pub category: Option<String>,
    /// Additional metadata.
    pub metadata: HashMap<String, Value>,
}

impl ToolDescriptor {
    /// Create a new tool descriptor.
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

    /// Set parameters schema.
    pub fn with_parameters(mut self, schema: Value) -> Self {
        self.parameters = schema;
        self
    }

    /// Set category.
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Add metadata.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Tool trait for implementing agent tools.
///
/// # Example
///
/// ```ignore
/// use tirea::contracts::runtime::tool_call::{Tool, ToolDescriptor, ToolExecutionEffect, ToolResult};
/// use tirea::contracts::runtime::state::AnyStateAction;
/// use tirea::contracts::ToolCallContext;
/// use tirea_state::State;
///
/// #[derive(State)]
/// struct MyToolState {
///     pub count: i64,
/// }
///
/// enum MyToolAction {
///     Increment,
/// }
///
/// struct CounterTool;
///
/// #[async_trait]
/// impl Tool for CounterTool {
///     fn descriptor(&self) -> ToolDescriptor {
///         ToolDescriptor::new("counter", "Counter", "Increment a counter")
///     }
///
///     async fn execute(
///         &self,
///         args: Value,
///         ctx: &ToolCallContext<'_>,
///     ) -> Result<ToolResult, ToolError> {
///         let current = ctx
///             .snapshot_of::<MyToolState>()
///             .map(|state| state.count)
///             .unwrap_or(0);
///
///         Ok(ToolExecutionEffect::from(
///             ToolResult::success("counter", json!({"count": current + 1}))
///         ).with_action(
///             AnyStateAction::new::<MyToolState>(MyToolAction::Increment)
///         ))
///     }
/// }
/// ```
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool descriptor.
    fn descriptor(&self) -> ToolDescriptor;

    /// Validate tool arguments against the descriptor's JSON Schema before execution.
    ///
    /// The default implementation uses [`validate_against_schema`] with
    /// `descriptor().parameters`. Override to customise or skip validation.
    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        validate_against_schema(&self.descriptor().parameters, args)
    }

    /// Execute the tool.
    ///
    /// # Arguments
    ///
    /// - `args`: Tool arguments as JSON value
    /// - `ctx`: Execution context for state access (framework extracts patch after execution).
    ///   `ctx.idempotency_key()` is the current `tool_call_id`.
    ///   Tools should use it as the idempotency key for side effects.
    ///
    /// # Returns
    ///
    /// Tool result or error
    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError>;

    /// Execute tool and return structured effects.
    ///
    /// The default implementation delegates to [`Tool::execute`] and wraps the
    /// result without converting any context writes into state actions.
    ///
    /// Tools that mutate persisted state should override this method and emit
    /// explicit typed actions via `AnyStateAction::new...`.
    async fn execute_effect(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let result = self.execute(args, _ctx).await?;
        Ok(ToolExecutionEffect::from(result))
    }
}

/// Validate a JSON value against a JSON Schema.
///
/// Returns `Ok(())` if the value conforms to the schema, or
/// `Err(ToolError::InvalidArguments)` with a description of all violations.
pub fn validate_against_schema(schema: &Value, args: &Value) -> Result<(), ToolError> {
    let validator = jsonschema::Validator::new(schema)
        .map_err(|e| ToolError::Internal(format!("invalid tool schema: {e}")))?;
    if validator.is_valid(args) {
        return Ok(());
    }
    let errors: Vec<String> = validator.iter_errors(args).map(|e| e.to_string()).collect();
    Err(ToolError::InvalidArguments(errors.join("; ")))
}

// ---------------------------------------------------------------------------
// TypedTool – strongly-typed tool with automatic schema generation
// ---------------------------------------------------------------------------

/// Strongly-typed variant of [`Tool`] with automatic JSON Schema generation.
///
/// Implement this trait instead of [`Tool`] when your tool has a fixed
/// parameter shape. A blanket impl provides [`Tool`] automatically.
///
/// # Example
///
/// ```ignore
/// use serde::Deserialize;
/// use schemars::JsonSchema;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct GreetArgs {
///     name: String,
/// }
///
/// struct GreetTool;
///
/// #[async_trait]
/// impl TypedTool for GreetTool {
///     type Args = GreetArgs;
///     fn tool_id(&self) -> &str { "greet" }
///     fn name(&self) -> &str { "Greet" }
///     fn description(&self) -> &str { "Greet a user" }
///
///     async fn execute(&self, args: GreetArgs, _ctx: &ToolCallContext<'_>)
///         -> Result<ToolResult, ToolError>
///     {
///         Ok(ToolResult::success("greet", json!({"greeting": format!("Hello, {}!", args.name)})))
///     }
/// }
/// ```
#[async_trait]
pub trait TypedTool: Send + Sync {
    /// Argument type — must derive `Deserialize` and `JsonSchema`.
    type Args: for<'de> Deserialize<'de> + JsonSchema + Send;

    /// Unique tool id (snake_case).
    fn tool_id(&self) -> &str;

    /// Human-readable tool name.
    fn name(&self) -> &str;

    /// Tool description shown to the LLM.
    fn description(&self) -> &str;

    /// Optional business-logic validation after deserialization.
    ///
    /// Return `Err(message)` to reject with [`ToolError::InvalidArguments`].
    fn validate(&self, _args: &Self::Args) -> Result<(), String> {
        Ok(())
    }

    /// Execute with typed arguments.
    async fn execute(
        &self,
        args: Self::Args,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError>;
}

#[async_trait]
impl<T: TypedTool> Tool for T {
    fn descriptor(&self) -> ToolDescriptor {
        let schema = typed_tool_schema::<T::Args>();
        ToolDescriptor::new(self.tool_id(), self.name(), self.description()).with_parameters(schema)
    }

    /// Skips JSON Schema validation — `from_value` deserialization covers it.
    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let typed: T::Args =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.validate(&typed).map_err(ToolError::InvalidArguments)?;
        TypedTool::execute(self, typed, ctx).await
    }
}

/// Generate a JSON Schema `Value` from a type implementing `JsonSchema`.
fn typed_tool_schema<T: JsonSchema>() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));
    // Strip the $schema key — LLM providers don't need it.
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::phase::SuspendTicket;
    use crate::runtime::state::AnyStateAction;
    use crate::runtime::state::StateSpec;
    use crate::runtime::Suspension;
    use crate::runtime::{PendingToolCall, ToolCallResumeMode};
    use crate::testing::TestFixtureState;
    use serde_json::json;
    use tirea_state::{DocCell, PatchSink, Path as TPath, State, TireaResult};

    // =========================================================================
    // ToolError tests
    // =========================================================================

    #[test]
    fn test_tool_error_invalid_arguments() {
        let err = ToolError::InvalidArguments("missing field".to_string());
        assert_eq!(err.to_string(), "Invalid arguments: missing field");
    }

    #[test]
    fn test_tool_error_execution_failed() {
        let err = ToolError::ExecutionFailed("timeout".to_string());
        assert_eq!(err.to_string(), "Execution failed: timeout");
    }

    #[test]
    fn test_tool_error_denied() {
        let err = ToolError::Denied("no access".to_string());
        assert_eq!(err.to_string(), "Denied: no access");
    }

    #[test]
    fn test_tool_error_not_found() {
        let err = ToolError::NotFound("file.txt".to_string());
        assert_eq!(err.to_string(), "Not found: file.txt");
    }

    #[test]
    fn test_tool_error_internal() {
        let err = ToolError::Internal("unexpected".to_string());
        assert_eq!(err.to_string(), "Internal error: unexpected");
    }

    // =========================================================================
    // ToolStatus tests
    // =========================================================================

    #[test]
    fn test_tool_status_serialization() {
        assert_eq!(
            serde_json::to_string(&ToolStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&ToolStatus::Warning).unwrap(),
            "\"warning\""
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
    fn test_tool_status_deserialization() {
        assert_eq!(
            serde_json::from_str::<ToolStatus>("\"success\"").unwrap(),
            ToolStatus::Success
        );
        assert_eq!(
            serde_json::from_str::<ToolStatus>("\"warning\"").unwrap(),
            ToolStatus::Warning
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
    fn test_tool_status_equality() {
        assert_eq!(ToolStatus::Success, ToolStatus::Success);
        assert_ne!(ToolStatus::Success, ToolStatus::Error);
    }

    #[test]
    fn test_tool_status_clone() {
        let status = ToolStatus::Warning;
        let cloned = status.clone();
        assert_eq!(status, cloned);
    }

    #[test]
    fn test_tool_status_debug() {
        assert_eq!(format!("{:?}", ToolStatus::Success), "Success");
        assert_eq!(format!("{:?}", ToolStatus::Error), "Error");
    }

    // =========================================================================
    // ToolResult tests
    // =========================================================================

    #[test]
    fn test_tool_result_success() {
        let result = ToolResult::success("my_tool", json!({"value": 42}));
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.data, json!({"value": 42}));
        assert!(result.message.is_none());
        assert!(result.metadata.is_empty());
        assert!(result.is_success());
        assert!(!result.is_error());
        assert!(!result.is_pending());
    }

    #[test]
    fn test_tool_result_success_with_message() {
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
    fn test_tool_result_error() {
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
    fn test_tool_result_error_with_code() {
        let result = ToolResult::error_with_code("my_tool", "invalid_arguments", "missing input");
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(
            result.data,
            json!({
                "error": {
                    "code": "invalid_arguments",
                    "message": "missing input"
                }
            })
        );
        assert_eq!(
            result.message,
            Some("[invalid_arguments] missing input".to_string())
        );
        assert!(result.is_error());
    }

    #[test]
    fn test_tool_result_pending() {
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
    fn test_tool_result_with_suspension_roundtrip() {
        let suspension = SuspendTicket::new(
            Suspension::new("call_1", "tool:confirm")
                .with_message("Need confirmation")
                .with_parameters(json!({"message":"hi"})),
            PendingToolCall::new("call_1", "confirm", json!({"message":"hi"})),
            ToolCallResumeMode::ReplayToolCall,
        );
        let result = ToolResult::suspended_with("confirm", "waiting", suspension.clone());

        assert!(result.is_pending());
        assert_eq!(result.suspension(), Some(suspension));
    }

    #[test]
    fn test_tool_result_warning() {
        let result = ToolResult::warning("my_tool", json!({"partial": true}), "Some items skipped");
        assert_eq!(result.tool_name, "my_tool");
        assert_eq!(result.status, ToolStatus::Warning);
        assert_eq!(result.data, json!({"partial": true}));
        assert_eq!(result.message, Some("Some items skipped".to_string()));
        // Warning is considered success
        assert!(result.is_success());
        assert!(!result.is_error());
    }

    #[test]
    fn test_tool_result_with_metadata() {
        let result = ToolResult::success("my_tool", json!({}))
            .with_metadata("duration_ms", 150)
            .with_metadata("retry_count", 2);
        assert_eq!(result.metadata.get("duration_ms"), Some(&json!(150)));
        assert_eq!(result.metadata.get("retry_count"), Some(&json!(2)));
    }

    #[test]
    fn test_tool_result_serialization() {
        let result =
            ToolResult::success("my_tool", json!({"key": "value"})).with_metadata("extra", "data");

        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.tool_name, "my_tool");
        assert_eq!(parsed.status, ToolStatus::Success);
        assert_eq!(parsed.data, json!({"key": "value"}));
    }

    #[test]
    fn test_tool_result_clone() {
        let result = ToolResult::success("my_tool", json!({"x": 1}));
        let cloned = result.clone();
        assert_eq!(result.tool_name, cloned.tool_name);
        assert_eq!(result.status, cloned.status);
    }

    #[test]
    fn test_tool_result_debug() {
        let result = ToolResult::success("test", json!(null));
        let debug = format!("{:?}", result);
        assert!(debug.contains("ToolResult"));
        assert!(debug.contains("test"));
    }

    // =========================================================================
    // ToolDescriptor tests
    // =========================================================================

    #[test]
    fn test_tool_descriptor_new() {
        let desc = ToolDescriptor::new("read_file", "Read File", "Reads a file from disk");
        assert_eq!(desc.id, "read_file");
        assert_eq!(desc.name, "Read File");
        assert_eq!(desc.description, "Reads a file from disk");
        assert!(desc.category.is_none());
        assert!(desc.metadata.is_empty());
        // Default parameters
        assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn test_tool_descriptor_with_parameters() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        });
        let desc =
            ToolDescriptor::new("read_file", "Read File", "Read").with_parameters(schema.clone());
        assert_eq!(desc.parameters, schema);
    }

    #[test]
    fn test_tool_descriptor_with_category() {
        let desc =
            ToolDescriptor::new("read_file", "Read File", "Read").with_category("filesystem");
        assert_eq!(desc.category, Some("filesystem".to_string()));
    }

    #[test]
    fn test_tool_descriptor_with_metadata() {
        let desc = ToolDescriptor::new("my_tool", "My Tool", "Description")
            .with_metadata("version", "1.0")
            .with_metadata("author", "test");
        assert_eq!(desc.metadata.get("version"), Some(&json!("1.0")));
        assert_eq!(desc.metadata.get("author"), Some(&json!("test")));
    }

    #[test]
    fn test_tool_descriptor_builder_chain() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc")
            .with_parameters(json!({"type": "object"}))
            .with_category("test")
            .with_metadata("key", "value");

        assert_eq!(desc.id, "tool");
        assert_eq!(desc.category, Some("test".to_string()));
        assert_eq!(desc.metadata.get("key"), Some(&json!("value")));
    }

    #[test]
    fn test_tool_descriptor_serialization() {
        let desc =
            ToolDescriptor::new("my_tool", "My Tool", "Does things").with_category("utilities");

        let json = serde_json::to_string(&desc).unwrap();
        let parsed: ToolDescriptor = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "my_tool");
        assert_eq!(parsed.name, "My Tool");
        assert_eq!(parsed.category, Some("utilities".to_string()));
    }

    #[test]
    fn test_tool_descriptor_clone() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc").with_category("cat");
        let cloned = desc.clone();
        assert_eq!(desc.id, cloned.id);
        assert_eq!(desc.category, cloned.category);
    }

    #[test]
    fn test_tool_descriptor_debug() {
        let desc = ToolDescriptor::new("tool", "Tool", "Desc");
        let debug = format!("{:?}", desc);
        assert!(debug.contains("ToolDescriptor"));
        assert!(debug.contains("tool"));
    }

    // =========================================================================
    // validate_against_schema tests
    // =========================================================================

    #[test]
    fn test_validate_against_schema_valid() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        assert!(validate_against_schema(&schema, &json!({"name": "Alice"})).is_ok());
    }

    #[test]
    fn test_validate_against_schema_missing_required() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let err = validate_against_schema(&schema, &json!({})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn test_validate_against_schema_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" }
            },
            "required": ["count"]
        });
        let err = validate_against_schema(&schema, &json!({"count": "not_a_number"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn test_validate_against_schema_empty_schema_accepts_object() {
        let schema = json!({"type": "object", "properties": {}});
        assert!(validate_against_schema(&schema, &json!({"anything": true})).is_ok());
    }

    #[test]
    fn test_validate_against_schema_multiple_errors_joined() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age":  { "type": "integer" }
            },
            "required": ["name", "age"]
        });
        let err = validate_against_schema(&schema, &json!({})).unwrap_err();
        let msg = err.to_string();
        // Both missing-field errors should be present, joined by "; "
        assert!(
            msg.contains("; "),
            "expected multiple errors joined by '; ', got: {msg}"
        );
        assert!(msg.contains("name"), "expected 'name' in error: {msg}");
        assert!(msg.contains("age"), "expected 'age' in error: {msg}");
    }

    #[test]
    fn test_validate_against_schema_null_args_rejected() {
        let schema = json!({"type": "object", "properties": {}});
        let err = validate_against_schema(&schema, &json!(null)).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn test_validate_against_schema_invalid_schema_returns_internal() {
        // "type" must be a string — passing an integer makes the schema itself invalid.
        let bad_schema = json!({"type": 123});
        let err = validate_against_schema(&bad_schema, &json!({})).unwrap_err();
        assert!(
            matches!(err, ToolError::Internal(_)),
            "expected Internal error for invalid schema, got: {err}"
        );
    }

    #[test]
    fn test_validate_against_schema_nested_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "object",
                    "properties": {
                        "city": { "type": "string" }
                    },
                    "required": ["city"]
                }
            },
            "required": ["address"]
        });
        // Valid nested
        assert!(validate_against_schema(&schema, &json!({"address": {"city": "Berlin"}})).is_ok());
        // Missing nested required field
        let err = validate_against_schema(&schema, &json!({"address": {}})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        // Wrong nested type
        let err = validate_against_schema(&schema, &json!({"address": {"city": 42}})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    // =========================================================================
    // TypedTool tests
    // =========================================================================

    #[derive(Deserialize, JsonSchema)]
    struct GreetArgs {
        name: String,
    }

    struct GreetTool;

    #[async_trait]
    impl TypedTool for GreetTool {
        type Args = GreetArgs;
        fn tool_id(&self) -> &str {
            "greet"
        }
        fn name(&self) -> &str {
            "Greet"
        }
        fn description(&self) -> &str {
            "Greet a user"
        }

        async fn execute(
            &self,
            args: GreetArgs,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "greet",
                json!({"greeting": format!("Hello, {}!", args.name)}),
            ))
        }
    }

    #[test]
    fn test_typed_tool_descriptor_schema() {
        let tool = GreetTool;
        let desc = Tool::descriptor(&tool);
        assert_eq!(desc.id, "greet");
        assert_eq!(desc.name, "Greet");
        assert_eq!(desc.description, "Greet a user");

        let props = desc.parameters.get("properties").unwrap();
        assert!(props.get("name").is_some());
        let required = desc.parameters.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
        // No $schema key
        assert!(desc.parameters.get("$schema").is_none());
    }

    #[tokio::test]
    async fn test_typed_tool_execute_success() {
        let tool = GreetTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        let result = Tool::execute(&tool, json!({"name": "World"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["greeting"], "Hello, World!");
    }

    #[tokio::test]
    async fn test_typed_tool_execute_deser_failure() {
        let tool = GreetTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        let err = Tool::execute(&tool, json!({"name": 123}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[derive(Deserialize, JsonSchema)]
    struct PositiveArgs {
        value: i64,
    }

    struct PositiveTool;

    #[async_trait]
    impl TypedTool for PositiveTool {
        type Args = PositiveArgs;
        fn tool_id(&self) -> &str {
            "positive"
        }
        fn name(&self) -> &str {
            "Positive"
        }
        fn description(&self) -> &str {
            "Requires positive value"
        }

        fn validate(&self, args: &PositiveArgs) -> Result<(), String> {
            if args.value <= 0 {
                return Err("value must be positive".into());
            }
            Ok(())
        }

        async fn execute(
            &self,
            args: PositiveArgs,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "positive",
                json!({"value": args.value}),
            ))
        }
    }

    #[tokio::test]
    async fn test_typed_tool_validate_rejection() {
        let tool = PositiveTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        let err = Tool::execute(&tool, json!({"value": -1}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("positive"));
    }

    #[test]
    fn test_typed_tool_as_arc_dyn_tool() {
        let tool: std::sync::Arc<dyn Tool> = std::sync::Arc::new(GreetTool);
        let desc = tool.descriptor();
        assert_eq!(desc.id, "greet");
    }

    #[test]
    fn test_typed_tool_skips_schema_validation() {
        let tool = GreetTool;
        // validate_args should always return Ok for TypedTool
        assert!(tool.validate_args(&json!({})).is_ok());
        assert!(tool.validate_args(&json!({"wrong": 123})).is_ok());
        assert!(tool.validate_args(&json!(null)).is_ok());
    }

    // -- TypedTool edge cases --------------------------------------------------

    #[derive(Deserialize, JsonSchema)]
    struct OptionalArgs {
        required_field: String,
        optional_field: Option<i64>,
    }

    struct OptionalTool;

    #[async_trait]
    impl TypedTool for OptionalTool {
        type Args = OptionalArgs;
        fn tool_id(&self) -> &str {
            "optional"
        }
        fn name(&self) -> &str {
            "Optional"
        }
        fn description(&self) -> &str {
            "Tool with optional field"
        }

        async fn execute(
            &self,
            args: OptionalArgs,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "optional",
                json!({
                    "required": args.required_field,
                    "optional": args.optional_field,
                }),
            ))
        }
    }

    #[tokio::test]
    async fn test_typed_tool_optional_field_absent() {
        let tool = OptionalTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        let result = Tool::execute(&tool, json!({"required_field": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["optional"], json!(null));
    }

    #[tokio::test]
    async fn test_typed_tool_extra_fields_ignored() {
        let tool = GreetTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        // serde ignores unknown fields by default
        let result = Tool::execute(&tool, json!({"name": "World", "extra": 999}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["greeting"], "Hello, World!");
    }

    #[tokio::test]
    async fn test_typed_tool_empty_json_all_required() {
        let tool = GreetTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");
        let err = Tool::execute(&tool, json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn test_default_execute_effect_wraps_execute_result() {
        let tool = GreetTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");

        let effect = Tool::execute_effect(&tool, json!({"name": "World"}), &ctx)
            .await
            .expect("execute_effect should succeed");

        assert_eq!(effect.result.tool_name, "greet");
        assert!(effect.result.is_success());
        let (_, actions) = effect.into_parts();
        assert!(actions.is_empty());
    }

    struct ContextWriteDefaultTool;

    #[async_trait]
    impl Tool for ContextWriteDefaultTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "context_write_default",
                "ContextWriteDefault",
                "writes state in execute",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            ctx.state_of::<TestFixtureState>()
                .set_label(Some("default_execute_write".to_string()))
                .expect("failed to set label");
            Ok(ToolResult::success(
                "context_write_default",
                json!({"ok": true}),
            ))
        }
    }

    #[tokio::test]
    async fn test_default_execute_effect_leaves_context_writes_in_context() {
        let tool = ContextWriteDefaultTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");

        let effect = Tool::execute_effect(&tool, json!({}), &ctx)
            .await
            .expect("execute_effect should succeed");

        assert!(effect.result.is_success());
        let (_, actions) = effect.into_parts();
        assert!(actions.is_empty());
        assert!(!ctx.take_patch().patch().is_empty());
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct ToolEffectState {
        value: i64,
    }

    struct ToolEffectStateRef;

    impl State for ToolEffectState {
        type Ref<'a> = ToolEffectStateRef;
        const PATH: &'static str = "tool_effect";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            ToolEffectStateRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for ToolEffectState {
        type Action = i64;

        fn reduce(&mut self, action: Self::Action) {
            self.value += action;
        }
    }

    struct EffectOnlyTool;

    #[async_trait]
    impl Tool for EffectOnlyTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("effect_only", "EffectOnly", "returns state actions")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("effect_only", json!({"ok": true})))
        }

        async fn execute_effect(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolExecutionEffect, ToolError> {
            Ok(
                ToolExecutionEffect::new(ToolResult::success("effect_only", json!({"ok": true})))
                    .with_action(AnyStateAction::new::<ToolEffectState>(1)),
            )
        }
    }

    #[tokio::test]
    async fn test_tool_can_return_state_actions_via_execute_effect() {
        let tool = EffectOnlyTool;
        let fixture = crate::testing::TestFixture::new();
        let ctx = fixture.ctx_with("call_1", "test");

        let effect = Tool::execute_effect(&tool, json!({}), &ctx)
            .await
            .expect("effect tool should succeed");

        assert!(effect.result.is_success());
        let (_, actions) = effect.into_parts();
        assert_eq!(actions.len(), 1);
        let boxed = actions.into_iter().next().unwrap();
        assert!(boxed.is_state_action());
        let sa = boxed
            .into_state_action()
            .expect("is_state_action returned true");
        assert!(sa.state_type_name().contains("ToolEffectState"));
    }
}
