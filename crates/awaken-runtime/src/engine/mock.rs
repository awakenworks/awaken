//! Mock LLM executor that returns canned responses without calling any API.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};

/// A mock LLM executor that returns canned responses without calling any API.
/// Used for testing and development.
pub struct MockLlmExecutor {
    responses: Vec<String>,
    default_response: String,
    index: AtomicUsize,
}

impl MockLlmExecutor {
    pub fn new() -> Self {
        Self {
            responses: vec![],
            default_response:
                "I'm a mock assistant. I'll help you test the system. What would you like to do?"
                    .into(),
            index: AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn with_responses(mut self, responses: Vec<String>) -> Self {
        self.responses = responses;
        self
    }

    fn next_response(&self) -> String {
        if self.responses.is_empty() {
            return self.default_response.clone();
        }
        let idx = self.index.fetch_add(1, Ordering::Relaxed) % self.responses.len();
        self.responses[idx].clone()
    }
}

impl Default for MockLlmExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmExecutor for MockLlmExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let text = self.next_response();
        Ok(StreamResult {
            content: vec![ContentBlock::text(text)],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(20),
                total_tokens: Some(30),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::Message;

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            upstream_model: "mock".into(),
            messages: vec![Message::user("hello")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    #[tokio::test]
    async fn default_response() {
        let executor = MockLlmExecutor::new();
        let result = executor.execute(make_request()).await.unwrap();
        assert!(result.text().contains("mock assistant"));
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(!result.needs_tools());
    }

    #[tokio::test]
    async fn cycles_through_responses() {
        let executor = MockLlmExecutor::new().with_responses(vec!["first".into(), "second".into()]);
        let r1 = executor.execute(make_request()).await.unwrap();
        assert_eq!(r1.text(), "first");
        let r2 = executor.execute(make_request()).await.unwrap();
        assert_eq!(r2.text(), "second");
        let r3 = executor.execute(make_request()).await.unwrap();
        assert_eq!(r3.text(), "first");
    }

    #[tokio::test]
    async fn usage_is_present() {
        let executor = MockLlmExecutor::new();
        let result = executor.execute(make_request()).await.unwrap();
        assert!(result.usage.is_some());
    }

    #[test]
    fn name_returns_mock() {
        let executor = MockLlmExecutor::new();
        assert_eq!(executor.name(), "mock");
    }

    #[tokio::test]
    async fn token_usage_in_response() {
        let executor = MockLlmExecutor::new();
        let result = executor.execute(make_request()).await.unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(30));
    }

    #[tokio::test]
    async fn stop_reason_is_end_turn() {
        let executor = MockLlmExecutor::new();
        let result = executor.execute(make_request()).await.unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn no_tool_calls_in_default_response() {
        let executor = MockLlmExecutor::new();
        let result = executor.execute(make_request()).await.unwrap();
        assert!(result.tool_calls.is_empty());
        assert!(!result.has_incomplete_tool_calls);
    }

    #[test]
    fn default_trait_matches_new() {
        let d = MockLlmExecutor::default();
        assert_eq!(d.name(), "mock");
        assert!(d.responses.is_empty());
    }

    #[tokio::test]
    async fn cycles_wraps_around_multiple_times() {
        let executor = MockLlmExecutor::new().with_responses(vec!["alpha".into(), "beta".into()]);
        // Go through 5 calls to verify wrapping
        let expected = ["alpha", "beta", "alpha", "beta", "alpha"];
        for exp in &expected {
            let result = executor.execute(make_request()).await.unwrap();
            assert_eq!(result.text(), *exp);
        }
    }

    #[tokio::test]
    async fn single_response_always_returns_same() {
        let executor = MockLlmExecutor::new().with_responses(vec!["only".into()]);
        for _ in 0..3 {
            let result = executor.execute(make_request()).await.unwrap();
            assert_eq!(result.text(), "only");
        }
    }

    #[tokio::test]
    async fn empty_responses_falls_back_to_default() {
        let executor = MockLlmExecutor::new().with_responses(vec![]);
        let result = executor.execute(make_request()).await.unwrap();
        assert!(result.text().contains("mock assistant"));
    }
}
