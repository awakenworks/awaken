//! Agent definition and configuration.

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::tool::Tool;
use std::collections::HashMap;
use std::sync::Arc;

use super::executor::{SequentialToolExecutor, ToolExecutor};

/// The sole interface the agent loop sees.
#[derive(Clone)]
pub struct AgentConfig {
    pub id: String,
    /// Model registry ID — the key used to look up the model entry.
    pub model_id: String,
    /// Actual model name for API calls (resolved from ModelEntry).
    pub model: String,
    pub system_prompt: String,
    pub max_rounds: usize,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub llm_executor: Arc<dyn LlmExecutor>,
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Context window management policy. `None` disables compaction and truncation.
    pub context_policy: Option<ContextWindowPolicy>,
    /// Context summarizer for LLM-based compaction. `None` disables LLM compaction
    /// (hard truncation still works if `context_policy` is set).
    pub context_summarizer: Option<Arc<dyn super::context::ContextSummarizer>>,
    /// Maximum number of continuation retries when the LLM response is truncated
    /// at `MaxTokens` with incomplete tool calls. `0` disables truncation recovery.
    pub max_continuation_retries: usize,
}

impl AgentConfig {
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        llm_executor: Arc<dyn LlmExecutor>,
    ) -> Self {
        let model = model.into();
        Self {
            id: id.into(),
            model_id: model.clone(),
            model,
            system_prompt: system_prompt.into(),
            max_rounds: 16,
            tools: HashMap::new(),
            llm_executor,
            tool_executor: Arc::new(SequentialToolExecutor),
            context_policy: None,
            context_summarizer: None,
            max_continuation_retries: 2,
        }
    }

    #[must_use]
    pub fn with_tool_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.tool_executor = executor;
        self
    }

    #[must_use]
    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        let desc = tool.descriptor();
        self.tools.insert(desc.id, tool);
        self
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        for tool in tools {
            let desc = tool.descriptor();
            self.tools.insert(desc.id, tool);
        }
        self
    }

    #[must_use]
    pub fn with_context_policy(mut self, policy: ContextWindowPolicy) -> Self {
        self.context_policy = Some(policy);
        self
    }

    #[must_use]
    pub fn with_context_summarizer(
        mut self,
        summarizer: Arc<dyn super::context::ContextSummarizer>,
    ) -> Self {
        self.context_summarizer = Some(summarizer);
        self
    }

    #[must_use]
    pub fn with_max_continuation_retries(mut self, n: usize) -> Self {
        self.max_continuation_retries = n;
        self
    }

    pub fn tool_descriptors(&self) -> Vec<awaken_contract::contract::tool::ToolDescriptor> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::tool::{ToolCallContext, ToolDescriptor, ToolError, ToolResult};
    use serde_json::{Value, json};

    struct MockLlm;

    #[async_trait]
    impl LlmExecutor for MockLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
        fn name(&self) -> &str {
            "mock"
        }
    }

    struct TestTool {
        id: String,
    }

    #[async_trait]
    impl Tool for TestTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(&self.id, &self.id, format!("{} tool", self.id))
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(&self.id, args))
        }
    }

    fn mock_executor() -> Arc<dyn LlmExecutor> {
        Arc::new(MockLlm)
    }

    #[test]
    fn config_new_defaults() {
        let config = AgentConfig::new("agent-1", "model-1", "system prompt", mock_executor());
        assert_eq!(config.id, "agent-1");
        assert_eq!(config.model, "model-1");
        assert_eq!(config.system_prompt, "system prompt");
        assert_eq!(config.max_rounds, 16);
        assert!(config.tools.is_empty());
        assert!(config.context_policy.is_none());
        assert!(config.context_summarizer.is_none());
        assert_eq!(config.max_continuation_retries, 2);
    }

    #[test]
    fn config_with_max_rounds() {
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_max_rounds(100);
        assert_eq!(config.max_rounds, 100);
    }

    #[test]
    fn config_with_tool() {
        let tool: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tool(tool);
        assert_eq!(config.tools.len(), 1);
        assert!(config.tools.contains_key("echo"));
    }

    #[test]
    fn config_with_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tools(tools);
        assert_eq!(config.tools.len(), 2);
        assert!(config.tools.contains_key("echo"));
        assert!(config.tools.contains_key("search"));
    }

    #[test]
    fn config_tool_descriptors() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descriptors = config.tool_descriptors();
        assert_eq!(descriptors.len(), 2);
        let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"echo"));
        assert!(ids.contains(&"search"));
    }

    #[test]
    fn config_with_context_policy() {
        use awaken_contract::contract::inference::ContextWindowPolicy;

        let policy = ContextWindowPolicy {
            max_context_tokens: 8000,
            max_output_tokens: 2000,
            min_recent_messages: 4,
            enable_prompt_cache: true,
            autocompact_threshold: Some(4096),
            compaction_mode: Default::default(),
            compaction_raw_suffix_messages: 3,
        };
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_context_policy(policy);
        assert!(config.context_policy.is_some());
        assert_eq!(
            config.context_policy.as_ref().unwrap().max_context_tokens,
            8000
        );
    }

    #[test]
    fn config_with_max_continuation_retries() {
        let config =
            AgentConfig::new("a", "m", "s", mock_executor()).with_max_continuation_retries(5);
        assert_eq!(config.max_continuation_retries, 5);
    }

    #[test]
    fn config_model_id_equals_model_by_default() {
        let config = AgentConfig::new("a", "claude-3", "s", mock_executor());
        assert_eq!(config.model_id, "claude-3");
        assert_eq!(config.model, "claude-3");
    }

    #[test]
    fn config_clone() {
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_max_rounds(50)
            .with_max_continuation_retries(3);
        let cloned = config.clone();
        assert_eq!(cloned.id, "a");
        assert_eq!(cloned.max_rounds, 50);
        assert_eq!(cloned.max_continuation_retries, 3);
    }

    #[test]
    fn config_with_tool_executor() {
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_tool_executor(Arc::new(super::super::executor::SequentialToolExecutor));
        assert_eq!(config.tool_executor.name(), "sequential");
    }

    #[test]
    fn config_chained_builder() {
        let config = AgentConfig::new("agent", "model", "system", mock_executor())
            .with_max_rounds(10)
            .with_max_continuation_retries(0)
            .with_tool(Arc::new(TestTool { id: "t1".into() }))
            .with_tool(Arc::new(TestTool { id: "t2".into() }));

        assert_eq!(config.max_rounds, 10);
        assert_eq!(config.max_continuation_retries, 0);
        assert_eq!(config.tools.len(), 2);
    }
}
