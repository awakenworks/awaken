//! LLM executor trait and tool execution strategy.

use super::content::ContentBlock;
use super::inference::{InferenceOverride, StreamResult};
use super::message::Message;
use super::tool::ToolDescriptor;
use async_trait::async_trait;
use thiserror::Error;

/// A provider-neutral LLM inference request.
#[derive(Debug, Clone)]
pub struct InferenceRequest {
    /// Model identifier.
    pub model: String,
    /// Messages to send.
    pub messages: Vec<Message>,
    /// Available tools.
    pub tools: Vec<ToolDescriptor>,
    /// System prompt content blocks. Empty means no system prompt.
    pub system: Vec<ContentBlock>,
    /// Per-inference overrides (temperature, max_tokens, etc).
    pub overrides: Option<InferenceOverride>,
}

/// Errors from LLM inference.
#[derive(Debug, Error)]
pub enum InferenceExecutionError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("cancelled")]
    Cancelled,
}

/// Abstraction over LLM inference backends.
///
/// Unlike uncarve's `LlmExecutor` which is coupled to the `genai` crate,
/// this trait uses awaken's own request/response types.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    /// Execute a chat completion and return the collected result.
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError>;

    /// Provider name for logging/debugging.
    fn name(&self) -> &str;
}

/// Tool execution strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// Execute tool calls one at a time.
    #[default]
    Sequential,
    /// Execute all tool calls concurrently, batch approval gate.
    ParallelBatchApproval,
    /// Execute all tool calls concurrently, streaming results.
    ParallelStreaming,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::inference::{StopReason, TokenUsage};
    use crate::contract::message::ToolCall;
    use crate::contract::tool::ToolDescriptor;
    use serde_json::json;

    /// A mock LLM executor for testing.
    struct MockLlm {
        response_text: String,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait]
    impl LlmExecutor for MockLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: if self.response_text.is_empty() {
                    vec![]
                } else {
                    vec![ContentBlock::text(self.response_text.clone())]
                },
                tool_calls: self.tool_calls.clone(),
                usage: Some(TokenUsage {
                    prompt_tokens: Some(100),
                    completion_tokens: Some(50),
                    total_tokens: Some(150),
                    ..Default::default()
                }),
                stop_reason: if self.tool_calls.is_empty() {
                    Some(StopReason::EndTurn)
                } else {
                    Some(StopReason::ToolUse)
                },
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn mock_llm_returns_text() {
        let llm = MockLlm {
            response_text: "Hello!".into(),
            tool_calls: vec![],
        };
        let request = InferenceRequest {
            model: "test-model".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            system: vec![],
            overrides: None,
        };
        let result = llm.execute(request).await.unwrap();
        assert_eq!(result.text(), "Hello!");
        assert!(!result.needs_tools());
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn mock_llm_returns_tool_calls() {
        let llm = MockLlm {
            response_text: String::new(),
            tool_calls: vec![ToolCall::new("c1", "search", json!({"q": "rust"}))],
        };
        let request = InferenceRequest {
            model: "test-model".into(),
            messages: vec![Message::user("search for rust")],
            tools: vec![ToolDescriptor::new("search", "search", "Web search")],
            system: vec![ContentBlock::text("You are helpful.")],
            overrides: None,
        };
        let result = llm.execute(request).await.unwrap();
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn mock_llm_with_overrides() {
        let llm = MockLlm {
            response_text: "ok".into(),
            tool_calls: vec![],
        };
        let request = InferenceRequest {
            model: "base-model".into(),
            messages: vec![],
            tools: vec![],
            system: vec![],
            overrides: Some(InferenceOverride {
                model: Some("override-model".into()),
                temperature: Some(0.7),
                ..Default::default()
            }),
        };
        let result = llm.execute(request).await.unwrap();
        assert_eq!(result.text(), "ok");
    }

    #[test]
    fn llm_executor_name_is_exposed() {
        let llm = MockLlm {
            response_text: String::new(),
            tool_calls: vec![],
        };

        assert_eq!(llm.name(), "mock");
    }

    #[test]
    fn tool_execution_mode_default_is_sequential() {
        assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Sequential);
    }

    #[test]
    fn inference_execution_error_display_strings_are_stable() {
        assert_eq!(
            InferenceExecutionError::Provider("provider failed".into()).to_string(),
            "provider error: provider failed"
        );
        assert_eq!(
            InferenceExecutionError::RateLimited("too many requests".into()).to_string(),
            "rate limited: too many requests"
        );
        assert_eq!(
            InferenceExecutionError::Timeout("slow backend".into()).to_string(),
            "timeout: slow backend"
        );
        assert_eq!(InferenceExecutionError::Cancelled.to_string(), "cancelled");
    }
}
