//! Agent definition and configuration.

use crate::contract::executor::LlmExecutor;
use crate::contract::inference::ContextWindowPolicy;
use crate::contract::tool::Tool;
use std::collections::HashMap;
use std::sync::Arc;

use super::executor::{SequentialToolExecutor, ToolExecutor};

/// The sole interface the agent loop sees.
pub struct AgentConfig {
    pub id: String,
    pub model: String,
    pub system_prompt: String,
    pub max_rounds: usize,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub llm_executor: Arc<dyn LlmExecutor>,
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Context window management policy. `None` disables compaction and truncation.
    pub context_policy: Option<ContextWindowPolicy>,
}

impl AgentConfig {
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        llm_executor: Arc<dyn LlmExecutor>,
    ) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            system_prompt: system_prompt.into(),
            max_rounds: 16,
            tools: HashMap::new(),
            llm_executor,
            tool_executor: Arc::new(SequentialToolExecutor),
            context_policy: None,
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

    pub fn tool_descriptors(&self) -> Vec<crate::contract::tool::ToolDescriptor> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }
}
