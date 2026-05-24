//! `config-assistant` agent factory.
//!
//! The admin console's `/assistant` page sends every chat turn to
//! `POST /v1/ai-sdk/agents/config-assistant/runs`. Without a seeded
//! agent the dispatcher returns "agent not found" and the page sits
//! silently. Seed a default-model, text-only agent here so the page
//! is usable on a fresh starter boot.

use awaken_contract::AgentSpec;

use crate::starter_backend::generative_ui_config::StarterPromptOverrides;

/// Build the default `config-assistant` agent. Bound to the `default`
/// model so it inherits whatever provider/key the operator already
/// configured. No special tools — the assistant guides config changes
/// by suggestion, the operator applies them via the admin UI.
pub fn config_assistant_agent(prompt_overrides: &StarterPromptOverrides) -> AgentSpec {
    super::apply_agent_prompt_override(
        AgentSpec {
            id: "config-assistant".into(),
            model_id: "default".into(),
            system_prompt: include_str!("config_assistant_prompt.txt").to_string(),
            max_rounds: 6,
            plugin_ids: vec!["permission".into()],
            ..Default::default()
        },
        prompt_overrides,
    )
}
