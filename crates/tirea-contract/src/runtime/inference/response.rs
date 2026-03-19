pub use crate::runtime::tool_call::{ToolResult, ToolStatus};
use crate::thread::ToolCall;
use serde::{Deserialize, Serialize};

/// Why the LLM stopped generating output.
///
/// Mapped from provider-specific stop reasons (Anthropic `stop_reason`,
/// OpenAI `finish_reason`). Used by plugins to detect truncation and
/// trigger recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StopReason {
    /// Model finished naturally (Anthropic `end_turn`, OpenAI `stop`).
    EndTurn,
    /// Output hit the `max_tokens` limit — response may be truncated.
    MaxTokens,
    /// Model emitted one or more tool-use calls.
    ToolUse,
    /// A stop sequence was matched.
    StopSequence,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_tokens: Option<i32>,
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
    /// Why the model stopped generating. `None` when the backend cannot
    /// determine or map the provider stop reason.
    pub stop_reason: Option<StopReason>,
}

impl StreamResult {
    /// Check if tool execution is needed.
    pub fn needs_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Inference error emitted by the loop and consumed by telemetry plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InferenceError {
    /// Stable error class used for metrics/telemetry dimensions.
    #[serde(rename = "type")]
    pub error_type: String,
    /// Human-readable error message.
    pub message: String,
    /// Classified error category (e.g. `rate_limit`, `timeout`, `connection`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

/// LLM response extension: set after inference completes (success or error).
#[derive(Debug, Clone)]
pub struct LLMResponse {
    /// Inference outcome: success with a [`StreamResult`] or failure with an [`InferenceError`].
    pub outcome: Result<StreamResult, InferenceError>,
}

impl LLMResponse {
    pub fn success(result: StreamResult) -> Self {
        Self {
            outcome: Ok(result),
        }
    }

    pub fn error(error: InferenceError) -> Self {
        Self {
            outcome: Err(error),
        }
    }
}
