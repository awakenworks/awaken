pub use crate::runtime::tool_call::{ToolResult, ToolStatus};
use crate::thread::ToolCall;
use serde::{Deserialize, Serialize};

/// Provider-neutral token usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<i32>,
}

/// Result of stream collection used by runtime and plugin phase contracts.
#[derive(Debug, Clone)]
pub struct StreamResult {
    /// Accumulated text content.
    pub text: String,
    /// Collected tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage from the LLM response.
    pub usage: Option<TokenUsage>,
}

impl StreamResult {
    /// Check if tool execution is needed.
    pub fn needs_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// LLM response extension: set after inference completes.
#[derive(Debug, Clone)]
pub struct LLMResponse {
    /// LLM inference result.
    pub result: StreamResult,
}

impl LLMResponse {
    pub fn new(result: StreamResult) -> Self {
        Self { result }
    }
}
