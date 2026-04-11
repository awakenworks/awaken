//! Agent resolution: dynamic lookup of agent config + execution environment.

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::execution::{SequentialToolExecutor, ToolExecutor};
use crate::phase::ExecutionEnv;

/// A fully resolved agent: all capabilities + plugin environment, ready to run.
///
/// Produced by `AgentResolver::resolve()`. Contains live references
/// (LlmExecutor, tools, plugins) — not serializable.
#[derive(Clone)]
pub struct ResolvedAgent {
    /// The source agent specification.
    pub spec: Arc<AgentSpec>,
    /// Actual model name sent to the upstream provider.
    pub upstream_model: String,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub llm_executor: Arc<dyn LlmExecutor>,
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Context summarizer for LLM-based compaction. `None` disables LLM compaction
    /// (hard truncation still works if `context_policy` is set).
    pub context_summarizer: Option<Arc<dyn crate::context::ContextSummarizer>>,
    /// Plugin-provided behavior (hooks, handlers, transforms).
    pub env: ExecutionEnv,
}

impl ResolvedAgent {
    /// Create a minimal ResolvedAgent (for tests).
    pub fn new(
        id: impl Into<String>,
        upstream_model: impl Into<String>,
        system_prompt: impl Into<String>,
        llm_executor: Arc<dyn LlmExecutor>,
    ) -> Self {
        let upstream_model = upstream_model.into();
        let spec = Arc::new(AgentSpec {
            id: id.into(),
            model_id: upstream_model.clone(),
            system_prompt: system_prompt.into(),
            max_rounds: 16,
            max_continuation_retries: 2,
            context_policy: None,
            reasoning_effort: None,
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
            upstream_model,
            tools: HashMap::new(),
            llm_executor,
            tool_executor: Arc::new(SequentialToolExecutor),
            context_summarizer: None,
            env: ExecutionEnv::empty(),
        }
    }

    // -- delegation accessors -------------------------------------------------

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn model_id(&self) -> &str {
        &self.spec.model_id
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

impl std::fmt::Debug for ResolvedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAgent")
            .field("agent_id", &self.id())
            .finish_non_exhaustive()
    }
}

/// Resolves an agent by ID, producing a ready-to-execute config + environment.
///
/// Implementations look up `AgentSpec` from a registry, resolve the model → provider
/// chain to obtain `LlmExecutor`, filter tools, install plugins, and build the
/// `ExecutionEnv`. The loop runner calls this at startup and at step boundaries
/// when handoff is detected (via `ActiveAgentKey`).
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError>;

    /// List known agent IDs for discovery endpoints.
    ///
    /// Implementations that cannot enumerate agents may return an empty list.
    fn agent_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::tool::{
        ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
    use serde_json::Value;

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
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success(&self.id, args).into())
        }
    }

    fn mock_executor() -> Arc<dyn LlmExecutor> {
        Arc::new(MockLlm)
    }

    #[test]
    fn new_defaults() {
        let agent = ResolvedAgent::new("agent-1", "model-1", "system prompt", mock_executor());
        assert_eq!(agent.id(), "agent-1");
        assert_eq!(agent.upstream_model, "model-1");
        assert_eq!(agent.system_prompt(), "system prompt");
        assert_eq!(agent.max_rounds(), 16);
        assert!(agent.tools.is_empty());
        assert!(agent.context_policy().is_none());
        assert!(agent.context_summarizer.is_none());
        assert_eq!(agent.max_continuation_retries(), 2);
    }

    #[test]
    fn with_max_rounds() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_max_rounds(100);
        assert_eq!(agent.max_rounds(), 100);
    }

    #[test]
    fn with_tool() {
        let tool: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tool(tool);
        assert_eq!(agent.tools.len(), 1);
        assert!(agent.tools.contains_key("echo"));
    }

    #[test]
    fn with_tools() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tools(tools);
        assert_eq!(agent.tools.len(), 2);
        assert!(agent.tools.contains_key("echo"));
        assert!(agent.tools.contains_key("search"));
    }

    #[test]
    fn tool_descriptors() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descriptors = agent.tool_descriptors();
        assert_eq!(descriptors.len(), 2);
        let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"echo"));
        assert!(ids.contains(&"search"));
    }

    #[test]
    fn with_context_policy() {
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
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_context_policy(policy);
        assert!(agent.context_policy().is_some());
        assert_eq!(agent.context_policy().unwrap().max_context_tokens, 8000);
    }

    #[test]
    fn with_max_continuation_retries() {
        let agent =
            ResolvedAgent::new("a", "m", "s", mock_executor()).with_max_continuation_retries(5);
        assert_eq!(agent.max_continuation_retries(), 5);
    }

    #[test]
    fn model_id_equals_model_by_default() {
        let agent = ResolvedAgent::new("a", "claude-3", "s", mock_executor());
        assert_eq!(agent.model_id(), "claude-3");
        assert_eq!(agent.upstream_model, "claude-3");
    }

    #[test]
    fn clone_works() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_max_rounds(50)
            .with_max_continuation_retries(3);
        let cloned = agent.clone();
        assert_eq!(cloned.id(), "a");
        assert_eq!(cloned.max_rounds(), 50);
        assert_eq!(cloned.max_continuation_retries(), 3);
    }

    #[test]
    fn with_tool_executor() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_tool_executor(Arc::new(crate::execution::SequentialToolExecutor));
        assert_eq!(agent.tool_executor.name(), "sequential");
    }

    #[test]
    fn chained_builder() {
        let agent = ResolvedAgent::new("agent", "model", "system", mock_executor())
            .with_max_rounds(10)
            .with_max_continuation_retries(0)
            .with_tool(Arc::new(TestTool { id: "t1".into() }))
            .with_tool(Arc::new(TestTool { id: "t2".into() }));

        assert_eq!(agent.max_rounds(), 10);
        assert_eq!(agent.max_continuation_retries(), 0);
        assert_eq!(agent.tools.len(), 2);
    }

    #[test]
    fn tool_descriptors_sorted_by_id() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "zebra".into() }),
            Arc::new(TestTool { id: "alpha".into() }),
            Arc::new(TestTool {
                id: "middle".into(),
            }),
        ];
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descriptors = agent.tool_descriptors();
        let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn tool_descriptors_empty() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor());
        let descriptors = agent.tool_descriptors();
        assert!(descriptors.is_empty());
    }

    #[test]
    fn duplicate_tool_id_overwrites() {
        let t1: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let t2: Arc<dyn Tool> = Arc::new(TestTool { id: "echo".into() });
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_tool(t1)
            .with_tool(t2);
        assert_eq!(agent.tools.len(), 1, "duplicate tool ID should overwrite");
    }

    #[test]
    fn with_tools_deduplicates_by_id() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool { id: "echo".into() }),
            Arc::new(TestTool {
                id: "search".into(),
            }),
        ];
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tools(tools);
        assert_eq!(agent.tools.len(), 2);
    }

    #[test]
    fn system_prompt_preserved_verbatim() {
        let prompt = "You are a helpful assistant.\nBe concise.\nDo not hallucinate.";
        let agent = ResolvedAgent::new("a", "m", prompt, mock_executor());
        assert_eq!(agent.system_prompt(), prompt);
    }

    #[test]
    fn with_context_summarizer() {
        use crate::context::ContextSummarizer;
        use crate::context::summarizer::SummarizationError;

        struct MockSummarizer;
        #[async_trait]
        impl ContextSummarizer for MockSummarizer {
            async fn summarize(
                &self,
                _transcript: &str,
                _previous_summary: Option<&str>,
                _executor: &dyn awaken_contract::contract::executor::LlmExecutor,
            ) -> Result<String, SummarizationError> {
                Ok("summary".into())
            }
        }

        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_context_summarizer(Arc::new(MockSummarizer));
        assert!(agent.context_summarizer.is_some());
    }

    #[test]
    fn default_max_continuation_retries() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor());
        assert_eq!(agent.max_continuation_retries(), 2);
    }

    #[test]
    fn zero_max_rounds() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_max_rounds(0);
        assert_eq!(agent.max_rounds(), 0);
    }

    #[test]
    fn builder_all_options_set() {
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
        let agent = ResolvedAgent::new("full-agent", "gpt-4", "Be helpful.", mock_executor())
            .with_max_rounds(32)
            .with_max_continuation_retries(10)
            .with_tool(Arc::new(TestTool { id: "a".into() }))
            .with_tool(Arc::new(TestTool { id: "b".into() }))
            .with_tool(Arc::new(TestTool { id: "c".into() }))
            .with_context_policy(policy)
            .with_tool_executor(Arc::new(crate::execution::SequentialToolExecutor));

        assert_eq!(agent.id(), "full-agent");
        assert_eq!(agent.upstream_model, "gpt-4");
        assert_eq!(agent.system_prompt(), "Be helpful.");
        assert_eq!(agent.max_rounds(), 32);
        assert_eq!(agent.max_continuation_retries(), 10);
        assert_eq!(agent.tools.len(), 3);
        assert!(agent.context_policy().is_some());
        assert_eq!(agent.context_policy().unwrap().max_context_tokens, 16000);
        assert_eq!(agent.tool_executor.name(), "sequential");
    }

    #[test]
    fn empty_system_prompt() {
        let agent = ResolvedAgent::new("a", "m", "", mock_executor());
        assert_eq!(agent.system_prompt(), "");
    }

    #[test]
    fn system_prompt_with_unicode() {
        let prompt = "You are \u{1F916} a helpful assistant. \u{2764}";
        let agent = ResolvedAgent::new("a", "m", prompt, mock_executor());
        assert_eq!(agent.system_prompt(), prompt);
    }

    #[test]
    fn tool_descriptors_single_tool() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_tool(Arc::new(TestTool { id: "only".into() }));
        let descs = agent.tool_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "only");
    }

    #[test]
    fn tool_descriptors_many_tools_sorted() {
        let ids = ["zeta", "beta", "alpha", "gamma", "delta"];
        let tools: Vec<Arc<dyn Tool>> = ids
            .iter()
            .map(|id| Arc::new(TestTool { id: (*id).into() }) as Arc<dyn Tool>)
            .collect();
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_tools(tools);
        let descs = agent.tool_descriptors();
        let sorted_ids: Vec<&str> = descs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(sorted_ids, vec!["alpha", "beta", "delta", "gamma", "zeta"]);
    }

    #[test]
    fn with_tools_then_with_tool_merges() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(TestTool { id: "a".into() }),
            Arc::new(TestTool { id: "b".into() }),
        ];
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor())
            .with_tools(tools)
            .with_tool(Arc::new(TestTool { id: "c".into() }));
        assert_eq!(agent.tools.len(), 3);
        assert!(agent.tools.contains_key("a"));
        assert!(agent.tools.contains_key("b"));
        assert!(agent.tools.contains_key("c"));
    }

    #[test]
    fn default_tool_executor_is_sequential() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor());
        assert_eq!(agent.tool_executor.name(), "sequential");
        assert!(agent.tool_executor.requires_incremental_state());
    }

    #[test]
    fn model_id_and_upstream_model_independent_after_creation() {
        let mut agent = ResolvedAgent::new("a", "original-model", "s", mock_executor());
        // Manually change upstream_model (simulating what resolve pipeline does)
        agent.upstream_model = "resolved-model-name".into();
        assert_eq!(agent.model_id(), "original-model");
        assert_eq!(agent.upstream_model, "resolved-model-name");
    }

    #[test]
    fn large_max_rounds() {
        let agent = ResolvedAgent::new("a", "m", "s", mock_executor()).with_max_rounds(usize::MAX);
        assert_eq!(agent.max_rounds(), usize::MAX);
    }

    #[test]
    fn zero_continuation_retries_disables_recovery() {
        let agent =
            ResolvedAgent::new("a", "m", "s", mock_executor()).with_max_continuation_retries(0);
        assert_eq!(agent.max_continuation_retries(), 0);
    }
}
