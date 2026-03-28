//! Agent definition and configuration.

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;
use std::collections::HashMap;
use std::sync::Arc;

use crate::execution::{SequentialToolExecutor, ToolExecutor};

/// The sole interface the agent loop sees.
#[derive(Clone)]
pub struct AgentConfig {
    /// The source agent specification.
    pub spec: Arc<AgentSpec>,
    /// Actual model name for API calls (resolved from ModelEntry, may differ from spec.model).
    pub model: String,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub llm_executor: Arc<dyn LlmExecutor>,
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Context summarizer for LLM-based compaction. `None` disables LLM compaction
    /// (hard truncation still works if `context_policy` is set).
    pub context_summarizer: Option<Arc<dyn crate::context::ContextSummarizer>>,
}

impl AgentConfig {
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        llm_executor: Arc<dyn LlmExecutor>,
    ) -> Self {
        let model = model.into();
        let spec = Arc::new(AgentSpec {
            id: id.into(),
            model: model.clone(),
            system_prompt: system_prompt.into(),
            max_rounds: 16,
            max_continuation_retries: 2,
            context_policy: None,
            plugin_ids: Vec::new(),
            active_hook_filter: Default::default(),
            allowed_tools: None,
            excluded_tools: None,
            endpoint: None,
            delegates: Vec::new(),
            sections: Default::default(),
            registry: None,
        });
        Self {
            spec,
            model,
            tools: HashMap::new(),
            llm_executor,
            tool_executor: Arc::new(SequentialToolExecutor),
            context_summarizer: None,
        }
    }

    // -- delegation accessors -------------------------------------------------

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn model_id(&self) -> &str {
        &self.spec.model
    }

    pub fn system_prompt(&self) -> &str {
        &self.spec.system_prompt
    }

    pub fn max_rounds(&self) -> usize {
        self.spec.max_rounds
    }

    pub fn context_policy(&self) -> Option<&ContextWindowPolicy> {
        self.spec.context_policy.as_ref()
    }

    pub fn max_continuation_retries(&self) -> usize {
        self.spec.max_continuation_retries
    }

    // -- builder methods ------------------------------------------------------

    #[must_use]
    pub fn with_tool_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.tool_executor = executor;
        self
    }

    #[must_use]
    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        let mut spec = (*self.spec).clone();
        spec.max_rounds = max_rounds;
        self.spec = Arc::new(spec);
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
        let mut spec = (*self.spec).clone();
        spec.context_policy = Some(policy);
        self.spec = Arc::new(spec);
        self
    }

    #[must_use]
    pub fn with_context_summarizer(
        mut self,
        summarizer: Arc<dyn crate::context::ContextSummarizer>,
    ) -> Self {
        self.context_summarizer = Some(summarizer);
        self
    }

    #[must_use]
    pub fn with_max_continuation_retries(mut self, n: usize) -> Self {
        let mut spec = (*self.spec).clone();
        spec.max_continuation_retries = n;
        self.spec = Arc::new(spec);
        self
    }

    pub fn tool_descriptors(&self) -> Vec<awaken_contract::contract::tool::ToolDescriptor> {
        let mut descs: Vec<_> = self.tools.values().map(|t| t.descriptor()).collect();
        descs.sort_by(|a, b| a.id.cmp(&b.id));
        descs
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
        assert_eq!(config.id(), "agent-1");
        assert_eq!(config.model, "model-1");
        assert_eq!(config.system_prompt(), "system prompt");
        assert_eq!(config.max_rounds(), 16);
        assert!(config.tools.is_empty());
        assert!(config.context_policy().is_none());
        assert!(config.context_summarizer.is_none());
        assert_eq!(config.max_continuation_retries(), 2);
    }

    #[test]
    fn config_with_max_rounds() {
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_max_rounds(100);
        assert_eq!(config.max_rounds(), 100);
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
        assert!(config.context_policy().is_some());
        assert_eq!(config.context_policy().unwrap().max_context_tokens, 8000);
    }

    #[test]
    fn config_with_max_continuation_retries() {
        let config =
            AgentConfig::new("a", "m", "s", mock_executor()).with_max_continuation_retries(5);
        assert_eq!(config.max_continuation_retries(), 5);
    }

    #[test]
    fn config_model_id_equals_model_by_default() {
        let config = AgentConfig::new("a", "claude-3", "s", mock_executor());
        assert_eq!(config.model_id(), "claude-3");
        assert_eq!(config.model, "claude-3");
    }

    #[test]
    fn config_clone() {
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_max_rounds(50)
            .with_max_continuation_retries(3);
        let cloned = config.clone();
        assert_eq!(cloned.id(), "a");
        assert_eq!(cloned.max_rounds(), 50);
        assert_eq!(cloned.max_continuation_retries(), 3);
    }

    #[test]
    fn config_with_tool_executor() {
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_tool_executor(Arc::new(crate::execution::SequentialToolExecutor));
        assert_eq!(config.tool_executor.name(), "sequential");
    }

    #[test]
    fn config_chained_builder() {
        let config = AgentConfig::new("agent", "model", "system", mock_executor())
            .with_max_rounds(10)
            .with_max_continuation_retries(0)
            .with_tool(Arc::new(TestTool { id: "t1".into() }))
            .with_tool(Arc::new(TestTool { id: "t2".into() }));

        assert_eq!(config.max_rounds(), 10);
        assert_eq!(config.max_continuation_retries(), 0);
        assert_eq!(config.tools.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional config tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_tool_descriptors_sorted_by_id() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "zebra".into() }),
            Arc::new(TestTool { id: "alpha".into() }),
            Arc::new(TestTool {
                id: "middle".into(),
            }),
        ];
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descriptors = config.tool_descriptors();
        let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn config_tool_descriptors_empty() {
        let config = AgentConfig::new("a", "m", "s", mock_executor());
        let descriptors = config.tool_descriptors();
        assert!(descriptors.is_empty());
    }

    #[test]
    fn config_duplicate_tool_id_overwrites() {
        let t1: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let t2: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_tool(t1)
            .with_tool(t2);
        assert_eq!(config.tools.len(), 1, "duplicate tool ID should overwrite");
    }

    #[test]
    fn config_with_tools_deduplicates_by_id() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tools(tools);
        assert_eq!(config.tools.len(), 2);
    }

    #[test]
    fn config_system_prompt_preserved_verbatim() {
        let prompt = "You are a helpful assistant.\nBe concise.\nDo not hallucinate.";
        let config = AgentConfig::new("a", "m", prompt, mock_executor());
        assert_eq!(config.system_prompt(), prompt);
    }

    #[test]
    fn config_with_context_summarizer() {
        use crate::context::ContextSummarizer;

        struct MockSummarizer;
        #[async_trait]
        impl ContextSummarizer for MockSummarizer {
            async fn summarize(
                &self,
                _transcript: &str,
                _previous_summary: Option<&str>,
                _executor: &dyn awaken_contract::contract::executor::LlmExecutor,
            ) -> Result<String, String> {
                Ok("summary".into())
            }
        }

        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_context_summarizer(Arc::new(MockSummarizer));
        assert!(config.context_summarizer.is_some());
    }

    #[test]
    fn config_default_max_continuation_retries() {
        let config = AgentConfig::new("a", "m", "s", mock_executor());
        assert_eq!(config.max_continuation_retries(), 2);
    }

    #[test]
    fn config_zero_max_rounds() {
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_max_rounds(0);
        assert_eq!(config.max_rounds(), 0);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: builder completeness, edge cases, tool ordering
    // -----------------------------------------------------------------------

    #[test]
    fn config_builder_all_options_set() {
        use awaken_contract::contract::inference::ContextWindowPolicy;

        let policy = ContextWindowPolicy {
            max_context_tokens: 16000,
            max_output_tokens: 4000,
            min_recent_messages: 8,
            enable_prompt_cache: false,
            autocompact_threshold: Some(8000),
            compaction_mode: Default::default(),
            compaction_raw_suffix_messages: 5,
        };
        let config = AgentConfig::new("full-agent", "gpt-4", "Be helpful.", mock_executor())
            .with_max_rounds(32)
            .with_max_continuation_retries(10)
            .with_tool(Arc::new(TestTool { id: "a".into() }))
            .with_tool(Arc::new(TestTool { id: "b".into() }))
            .with_tool(Arc::new(TestTool { id: "c".into() }))
            .with_context_policy(policy)
            .with_tool_executor(Arc::new(crate::execution::SequentialToolExecutor));

        assert_eq!(config.id(), "full-agent");
        assert_eq!(config.model, "gpt-4");
        assert_eq!(config.system_prompt(), "Be helpful.");
        assert_eq!(config.max_rounds(), 32);
        assert_eq!(config.max_continuation_retries(), 10);
        assert_eq!(config.tools.len(), 3);
        assert!(config.context_policy().is_some());
        assert_eq!(config.context_policy().unwrap().max_context_tokens, 16000);
        assert_eq!(config.tool_executor.name(), "sequential");
    }

    #[test]
    fn config_empty_system_prompt() {
        let config = AgentConfig::new("a", "m", "", mock_executor());
        assert_eq!(config.system_prompt(), "");
    }

    #[test]
    fn config_system_prompt_with_unicode() {
        let prompt = "You are \u{1F916} a helpful assistant. \u{2764}";
        let config = AgentConfig::new("a", "m", prompt, mock_executor());
        assert_eq!(config.system_prompt(), prompt);
    }

    #[test]
    fn config_tool_descriptors_single_tool() {
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_tool(Arc::new(TestTool { id: "only".into() }));
        let descs = config.tool_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "only");
    }

    #[test]
    fn config_tool_descriptors_many_tools_sorted() {
        let ids = vec!["zeta", "beta", "alpha", "gamma", "delta"];
        let tools: Vec<Arc<dyn Tool>> = ids
            .iter()
            .map(|id| Arc::new(TestTool { id: (*id).into() }) as Arc<dyn Tool>)
            .collect();
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descs = config.tool_descriptors();
        let sorted_ids: Vec<&str> = descs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(sorted_ids, vec!["alpha", "beta", "delta", "gamma", "zeta"]);
    }

    #[test]
    fn config_with_tools_then_with_tool_merges() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "a".into() }),
            Arc::new(TestTool { id: "b".into() }),
        ];
        let config = AgentConfig::new("a", "m", "s", mock_executor())
            .with_tools(tools)
            .with_tool(Arc::new(TestTool { id: "c".into() }));
        assert_eq!(config.tools.len(), 3);
        assert!(config.tools.contains_key("a"));
        assert!(config.tools.contains_key("b"));
        assert!(config.tools.contains_key("c"));
    }

    #[test]
    fn config_default_tool_executor_is_sequential() {
        let config = AgentConfig::new("a", "m", "s", mock_executor());
        assert_eq!(config.tool_executor.name(), "sequential");
        assert!(config.tool_executor.requires_incremental_state());
    }

    #[test]
    fn config_model_id_and_model_independent_after_creation() {
        let mut config = AgentConfig::new("a", "original-model", "s", mock_executor());
        // Manually change model_name (simulating what resolve pipeline does)
        config.model = "resolved-model-name".into();
        assert_eq!(config.model_id(), "original-model");
        assert_eq!(config.model, "resolved-model-name");
    }

    #[test]
    fn config_large_max_rounds() {
        let config = AgentConfig::new("a", "m", "s", mock_executor()).with_max_rounds(usize::MAX);
        assert_eq!(config.max_rounds(), usize::MAX);
    }

    #[test]
    fn config_zero_continuation_retries_disables_recovery() {
        let config =
            AgentConfig::new("a", "m", "s", mock_executor()).with_max_continuation_retries(0);
        assert_eq!(config.max_continuation_retries(), 0);
    }
}
