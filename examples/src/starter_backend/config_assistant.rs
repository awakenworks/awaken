//! Legacy demo-only `config-assistant` agent factory.
//!
//! The production admin console uses the server-managed Admin Assistant at
//! `POST /v1/admin/assistant/runs`. This seeded agent is kept for
//! `AWAKEN_SEED_PROFILE=demo` scenarios that need the older chat-through-agent
//! example flow.

use awaken_contract::AgentSpec;

use crate::starter_backend::generative_ui_config::StarterPromptOverrides;

/// Build the legacy demo `config-assistant` agent. Bound to the `default`
/// model so it inherits whatever provider/key the operator already configured.
/// No special tools: the assistant guides config changes by suggestion, and the
/// operator applies them through the admin UI.
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
