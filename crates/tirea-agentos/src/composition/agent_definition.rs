use super::stop_condition::StopConditionSpec;
use super::AgentDescriptor;
use crate::runtime::loop_runner::LlmRetryPolicy;
use genai::chat::ChatOptions;
use std::collections::HashMap;

/// Partial configuration overlay applied when switching agent modes.
///
/// Each `Option` field means "override this value from the base definition".
/// `None` means "keep the base value".
#[derive(Debug, Clone, Default)]
pub struct ModeOverlay {
    /// Override model identifier.
    pub model: Option<String>,
    /// Override model fallbacks.
    pub model_fallbacks: Option<Vec<String>>,
    /// Override system prompt.
    pub system_prompt: Option<String>,
    /// Override maximum rounds.
    pub max_rounds: Option<usize>,
    /// Override chat options.
    pub chat_options: Option<Option<ChatOptions>>,
    /// Override tool whitelist.
    pub allowed_tools: Option<Option<Vec<String>>>,
    /// Override tool blacklist.
    pub excluded_tools: Option<Option<Vec<String>>>,
    /// Override behavior references.
    pub behavior_ids: Option<Vec<String>>,
    /// Override stop condition specs.
    pub stop_condition_specs: Option<Vec<StopConditionSpec>>,
}

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
    /// Human-readable display name used in discovery surfaces.
    #[allow(dead_code)]
    pub name: Option<String>,
    /// Short description exposed to callers/models when this agent is discoverable.
    #[allow(dead_code)]
    pub description: Option<String>,
    /// Model identifier (e.g., "gpt-4", "claude-3-opus").
    pub model: String,
    /// Fallback model ids used when the primary model fails.
    pub model_fallbacks: Vec<String>,
    /// System prompt for the LLM.
    pub system_prompt: String,
    /// Maximum number of tool call rounds before stopping.
    pub max_rounds: usize,
    /// Tool execution strategy.
    pub tool_execution_mode: ToolExecutionMode,
    /// Chat options for the LLM.
    pub chat_options: Option<ChatOptions>,
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
    /// Named mode overlays. Each mode partially overrides the base config.
    pub modes: HashMap<String, ModeOverlay>,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            id: "default".to_string(),
            name: None,
            description: None,
            model: "gpt-4o-mini".to_string(),
            model_fallbacks: Vec::new(),
            system_prompt: String::new(),
            max_rounds: 10,
            tool_execution_mode: ToolExecutionMode::ParallelStreaming,
            chat_options: Some(
                ChatOptions::default()
                    .with_capture_usage(true)
                    .with_capture_reasoning_content(true)
                    .with_capture_tool_calls(true),
            ),
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
            modes: HashMap::new(),
        }
    }
}

impl std::fmt::Debug for AgentDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentDefinition")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("model", &self.model)
            .field("model_fallbacks", &self.model_fallbacks)
            .field(
                "system_prompt",
                &format!("[{} chars]", self.system_prompt.len()),
            )
            .field("max_rounds", &self.max_rounds)
            .field("tool_execution_mode", &self.tool_execution_mode)
            .field("chat_options", &self.chat_options)
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
            .field("modes", &self.modes.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AgentDefinition {
    /// Create a new instance with the given model id.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    /// Create a new instance with explicit id and model.
    pub fn with_id(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Set system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Set max rounds.
    #[must_use]
    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// Set chat options.
    #[must_use]
    pub fn with_chat_options(mut self, options: ChatOptions) -> Self {
        self.chat_options = Some(options);
        self
    }

    /// Set fallback model ids to try after the primary model.
    #[must_use]
    pub fn with_fallback_models(mut self, models: Vec<String>) -> Self {
        self.model_fallbacks = models;
        self
    }

    /// Add a single fallback model id.
    #[must_use]
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        self.model_fallbacks.push(model.into());
        self
    }

    /// Set LLM retry policy.
    #[must_use]
    pub fn with_llm_retry_policy(mut self, policy: LlmRetryPolicy) -> Self {
        self.llm_retry_policy = policy;
        self
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    #[must_use]
    pub fn display_description(&self) -> &str {
        self.description.as_deref().unwrap_or("")
    }

    #[must_use]
    pub fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor::new(self.id.clone())
            .with_name(self.display_name())
            .with_description(self.display_description())
    }

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

    /// Register a named mode overlay.
    #[must_use]
    pub fn with_mode(mut self, name: impl Into<String>, overlay: ModeOverlay) -> Self {
        self.modes.insert(name.into(), overlay);
        self
    }

    /// Apply a mode overlay, returning a new definition with overridden fields.
    #[must_use]
    pub fn with_overlay(mut self, overlay: &ModeOverlay) -> Self {
        if let Some(ref model) = overlay.model {
            self.model = model.clone();
        }
        if let Some(ref models) = overlay.model_fallbacks {
            self.model_fallbacks = models.clone();
        }
        if let Some(ref prompt) = overlay.system_prompt {
            self.system_prompt = prompt.clone();
        }
        if let Some(rounds) = overlay.max_rounds {
            self.max_rounds = rounds;
        }
        if let Some(ref opts) = overlay.chat_options {
            self.chat_options = opts.clone();
        }
        if let Some(ref tools) = overlay.allowed_tools {
            self.allowed_tools = tools.clone();
        }
        if let Some(ref tools) = overlay.excluded_tools {
            self.excluded_tools = tools.clone();
        }
        if let Some(ref ids) = overlay.behavior_ids {
            self.behavior_ids = ids.clone();
        }
        if let Some(ref specs) = overlay.stop_condition_specs {
            self.stop_condition_specs = specs.clone();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::resolve::normalize_definition_models_for_test;

    #[test]
    fn normalize_definition_trims_model_and_fallback_models() {
        let definition =
            AgentDefinition::new(" openai::gemini-2.5-flash ").with_fallback_models(vec![
                " gpt-4o-mini ".to_string(),
                "   ".to_string(),
                " claude-3-7-sonnet ".to_string(),
            ]);
        let normalized = normalize_definition_models_for_test(definition);

        assert_eq!(normalized.model, "openai::gemini-2.5-flash");
        assert_eq!(
            normalized.model_fallbacks,
            vec!["gpt-4o-mini".to_string(), "claude-3-7-sonnet".to_string()]
        );
    }

    #[test]
    fn with_overlay_overrides_specified_fields() {
        let base = AgentDefinition::with_id("agent-1", "claude-opus")
            .with_system_prompt("base prompt")
            .with_max_rounds(20);

        let overlay = ModeOverlay {
            model: Some("claude-haiku".to_string()),
            max_rounds: Some(5),
            ..Default::default()
        };

        let result = base.with_overlay(&overlay);
        assert_eq!(result.model, "claude-haiku");
        assert_eq!(result.max_rounds, 5);
        assert_eq!(result.system_prompt, "base prompt"); // not overridden
        assert_eq!(result.id, "agent-1"); // never overridden
    }

    #[test]
    fn with_overlay_preserves_none_fields() {
        let base = AgentDefinition::with_id("agent-1", "claude-opus")
            .with_allowed_tools(vec!["Read".to_string()]);

        let overlay = ModeOverlay::default(); // all None

        let result = base.with_overlay(&overlay);
        assert_eq!(result.model, "claude-opus");
        assert_eq!(result.allowed_tools, Some(vec!["Read".to_string()]));
    }

    #[test]
    fn with_overlay_can_clear_allowed_tools() {
        let base = AgentDefinition::with_id("agent-1", "claude-opus")
            .with_allowed_tools(vec!["Read".to_string()]);

        let overlay = ModeOverlay {
            allowed_tools: Some(None), // explicitly clear
            ..Default::default()
        };

        let result = base.with_overlay(&overlay);
        assert!(result.allowed_tools.is_none());
    }

    #[test]
    fn with_mode_registers_named_overlay() {
        let definition = AgentDefinition::new("claude-opus").with_mode(
            "fast",
            ModeOverlay {
                model: Some("claude-haiku".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(definition.modes.len(), 1);
        assert!(definition.modes.contains_key("fast"));
        assert_eq!(
            definition.modes["fast"].model.as_deref(),
            Some("claude-haiku")
        );
    }
}
