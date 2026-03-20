//! Tool descriptor and result types.
//!
//! The async `Tool` trait is defined in a later commit (Stage 6) to avoid
//! pulling in `async-trait` and `futures` before the loop exists.

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
            metadata: HashMap::new(),
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
        }
    }

    /// Add metadata.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
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

/// Tool descriptor with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: Value,
    pub category: Option<String>,
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
    fn tool_result_with_metadata() {
        let result = ToolResult::success("calc", json!(42)).with_metadata("duration_ms", json!(15));
        assert_eq!(result.metadata["duration_ms"], json!(15));
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
            .with_category("math")
            .with_metadata("version", json!("1.0"));

        assert_eq!(desc.id, "calc");
        assert_eq!(desc.name, "calculator");
        assert_eq!(desc.category.as_deref(), Some("math"));
        assert_eq!(desc.metadata["version"], json!("1.0"));
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
    fn tool_result_to_json() {
        let result = ToolResult::success("calc", json!(42));
        let value = result.to_json();
        assert_eq!(value["tool_name"], "calc");
        assert_eq!(value["status"], "success");
    }
}
