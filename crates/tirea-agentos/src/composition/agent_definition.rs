use super::StopConditionSpec;
use crate::loop_runtime::loop_runner::LlmRetryPolicy;
use genai::chat::ChatOptions;

/// Tool execution strategy mode exposed by AgentDefinition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    ParallelBatchApproval,
    ParallelStreaming,
}

/// Agent composition definition owned by AgentOS.
///
/// This is the orchestration-facing model and uses only registry references
/// (`behavior_ids`, `stop_condition_ids`) and declarative specs.
/// Before execution, AgentOS resolves it into loop-facing [`BaseAgent`].
#[derive(Clone)]
pub struct AgentDefinition {
    /// Unique identifier for this agent.
    pub id: String,
    /// Model identifier (e.g., "gpt-4", "claude-3-opus").
    pub model: String,
    /// System prompt for the LLM.
    pub system_prompt: String,
    /// Maximum number of tool call rounds before stopping.
    pub max_rounds: usize,
    /// Tool execution strategy.
    pub tool_execution_mode: ToolExecutionMode,
    /// Chat options for the LLM.
    pub chat_options: Option<ChatOptions>,
    /// Fallback model ids used when the primary model fails.
    ///
    /// Evaluated in order after `model`.
    pub fallback_models: Vec<String>,
    /// Retry policy for LLM inference failures.
    pub llm_retry_policy: LlmRetryPolicy,
    /// Behavior references resolved from AgentOS behavior registry.
    pub behavior_ids: Vec<String>,
    /// Tool whitelist (None = all tools available).
    pub allowed_tools: Option<Vec<String>>,
    /// Tool blacklist.
    pub excluded_tools: Option<Vec<String>>,
    /// Skill whitelist (None = all skills available).
    pub allowed_skills: Option<Vec<String>>,
    /// Skill blacklist.
    pub excluded_skills: Option<Vec<String>>,
    /// Agent whitelist for `agent_run` delegation (None = all visible agents available).
    pub allowed_agents: Option<Vec<String>>,
    /// Agent blacklist for `agent_run` delegation.
    pub excluded_agents: Option<Vec<String>>,
    /// Declarative stop condition specs, resolved at runtime.
    pub stop_condition_specs: Vec<StopConditionSpec>,
    /// Stop condition references resolved from AgentOS StopPolicyRegistry.
    pub stop_condition_ids: Vec<String>,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            id: "default".to_string(),
            model: "gpt-4o-mini".to_string(),
            system_prompt: String::new(),
            max_rounds: 10,
            tool_execution_mode: ToolExecutionMode::ParallelStreaming,
            chat_options: Some(
                ChatOptions::default()
                    .with_capture_usage(true)
                    .with_capture_reasoning_content(true)
                    .with_capture_tool_calls(true),
            ),
            fallback_models: Vec::new(),
            llm_retry_policy: LlmRetryPolicy::default(),
            behavior_ids: Vec::new(),
            allowed_tools: None,
            excluded_tools: None,
            allowed_skills: None,
            excluded_skills: None,
            allowed_agents: None,
            excluded_agents: None,
            stop_condition_specs: Vec::new(),
            stop_condition_ids: Vec::new(),
        }
    }
}

impl std::fmt::Debug for AgentDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentDefinition")
            .field("id", &self.id)
            .field("model", &self.model)
            .field(
                "system_prompt",
                &format!("[{} chars]", self.system_prompt.len()),
            )
            .field("max_rounds", &self.max_rounds)
            .field("tool_execution_mode", &self.tool_execution_mode)
            .field("chat_options", &self.chat_options)
            .field("fallback_models", &self.fallback_models)
            .field("llm_retry_policy", &self.llm_retry_policy)
            .field("behavior_ids", &self.behavior_ids)
            .field("allowed_tools", &self.allowed_tools)
            .field("excluded_tools", &self.excluded_tools)
            .field("allowed_skills", &self.allowed_skills)
            .field("excluded_skills", &self.excluded_skills)
            .field("allowed_agents", &self.allowed_agents)
            .field("excluded_agents", &self.excluded_agents)
            .field("stop_condition_specs", &self.stop_condition_specs)
            .field("stop_condition_ids", &self.stop_condition_ids)
            .finish()
    }
}

impl AgentDefinition {
    tirea_contract::impl_shared_agent_builder_methods!();

    #[must_use]
    pub fn with_stop_condition_spec(mut self, spec: StopConditionSpec) -> Self {
        self.stop_condition_specs.push(spec);
        self
    }

    #[must_use]
    pub fn with_stop_condition_specs(mut self, specs: Vec<StopConditionSpec>) -> Self {
        self.stop_condition_specs = specs;
        self
    }

    #[must_use]
    pub fn with_tool_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.tool_execution_mode = mode;
        self
    }

    #[must_use]
    pub fn with_behavior_ids(mut self, behavior_ids: Vec<String>) -> Self {
        self.behavior_ids = behavior_ids;
        self
    }

    #[must_use]
    pub fn with_behavior_id(mut self, behavior_id: impl Into<String>) -> Self {
        self.behavior_ids.push(behavior_id.into());
        self
    }

    #[must_use]
    pub fn with_stop_condition_id(mut self, id: impl Into<String>) -> Self {
        self.stop_condition_ids.push(id.into());
        self
    }

    #[must_use]
    pub fn with_stop_condition_ids(mut self, ids: Vec<String>) -> Self {
        self.stop_condition_ids = ids;
        self
    }

    #[must_use]
    pub fn with_allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = Some(tools);
        self
    }

    #[must_use]
    pub fn with_excluded_tools(mut self, tools: Vec<String>) -> Self {
        self.excluded_tools = Some(tools);
        self
    }

    #[must_use]
    pub fn with_allowed_skills(mut self, skills: Vec<String>) -> Self {
        self.allowed_skills = Some(skills);
        self
    }

    #[must_use]
    pub fn with_excluded_skills(mut self, skills: Vec<String>) -> Self {
        self.excluded_skills = Some(skills);
        self
    }

    #[must_use]
    pub fn with_allowed_agents(mut self, agents: Vec<String>) -> Self {
        self.allowed_agents = Some(agents);
        self
    }

    #[must_use]
    pub fn with_excluded_agents(mut self, agents: Vec<String>) -> Self {
        self.excluded_agents = Some(agents);
        self
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::resolve::normalize_definition_models_for_test;

    #[test]
    fn normalize_definition_trims_model_and_fallback_models() {
        let definition = AgentDefinition::new(" openai::gemini-2.5-flash ")
            .with_fallback_models(vec![
                " gpt-4o-mini ".to_string(),
                "   ".to_string(),
                " claude-3-7-sonnet ".to_string(),
            ]);
        let normalized = normalize_definition_models_for_test(definition);

        assert_eq!(normalized.model, "openai::gemini-2.5-flash");
        assert_eq!(
            normalized.fallback_models,
            vec!["gpt-4o-mini".to_string(), "claude-3-7-sonnet".to_string()]
        );
    }
}
