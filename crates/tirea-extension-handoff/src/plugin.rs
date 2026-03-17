use super::actions::activate_handoff_action;
use super::state::HandoffState;
use async_trait::async_trait;
use std::collections::HashMap;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::overlay::AgentOverlay;
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction, BeforeToolExecuteAction};

/// Stable plugin id for handoff behavior.
pub const HANDOFF_PLUGIN_ID: &str = "agent_handoff";

/// Tools that are always allowed regardless of handoff tool restrictions.
const ALWAYS_ALLOWED_TOOLS: &[&str] = &["agent_handoff"];

/// Dynamic agent handoff plugin.
///
/// Applies agent overlays dynamically within the running agent loop:
///
/// 1. `before_inference`: reads active agent variant, decomposes the
///    [`AgentOverlay`] into `OverrideInference`, `AddSystemContext`,
///    `ExcludeTool`, `IncludeOnlyTools` actions
/// 2. `before_tool_execute`: enforces `allowed_tools` whitelist as a hard gate
///
/// No termination or re-resolution occurs — handoff is instant.
pub struct HandoffPlugin {
    overlays: HashMap<String, AgentOverlay>,
}

impl HandoffPlugin {
    /// Create a new handoff plugin with the given agent variant overlays.
    pub fn new(overlays: HashMap<String, AgentOverlay>) -> Self {
        Self { overlays }
    }
}

#[async_trait]
impl AgentBehavior for HandoffPlugin {
    fn id(&self) -> &str {
        HANDOFF_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(HandoffState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let state = ctx.snapshot_of::<HandoffState>().ok().unwrap_or_default();
        let mut actions = ActionSet::empty();

        // If a handoff was requested and differs from the active agent,
        // activate it immediately.
        if let Some(ref requested) = state.requested_agent {
            if state.active_agent.as_ref() != Some(requested) {
                actions = actions.and(ActionSet::single(BeforeInferenceAction::State(
                    activate_handoff_action(requested),
                )));
            }
        }

        // Determine the effective agent: prefer requested (just-switched),
        // fall back to active.
        let effective_agent = state
            .requested_agent
            .as_ref()
            .or(state.active_agent.as_ref());

        let Some(agent_name) = effective_agent else {
            return actions;
        };
        let Some(overlay) = self.overlays.get(agent_name.as_str()) else {
            return actions;
        };

        // Inject always-allowed tools into the overlay's allowed_tools
        // before decomposition, so agent_handoff is never filtered out.
        let mut effective_overlay = overlay.clone();
        if let Some(ref mut allowed) = effective_overlay.allowed_tools {
            for tool in ALWAYS_ALLOWED_TOOLS {
                if !allowed.iter().any(|t| t == tool) {
                    allowed.push(tool.to_string());
                }
            }
        }

        // Decompose overlay into standard BeforeInferenceAction variants.
        actions.and(effective_overlay.into_before_inference_actions())
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let state = ctx.snapshot_of::<HandoffState>().ok().unwrap_or_default();
        let effective_agent = state.active_agent.as_ref();

        let Some(agent_name) = effective_agent else {
            return ActionSet::empty();
        };
        let Some(overlay) = self.overlays.get(agent_name.as_str()) else {
            return ActionSet::empty();
        };

        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        // Handoff tools are always allowed
        if ALWAYS_ALLOWED_TOOLS.contains(&tool_id) {
            return ActionSet::empty();
        }

        // Hard gate: if allowed_tools whitelist is set, block anything not in it
        if let Some(ref allowed) = overlay.allowed_tools {
            if !allowed.iter().any(|t| t == tool_id) {
                return ActionSet::single(BeforeToolExecuteAction::Block(format!(
                    "Agent '{}' restricts tools. '{}' is not in the allowed list.",
                    agent_name, tool_id
                )));
            }
        }

        ActionSet::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::inference::InferenceOverride;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    fn test_plugin() -> HandoffPlugin {
        let mut overlays = HashMap::new();
        overlays.insert(
            "fast".to_string(),
            AgentOverlay {
                inference: Some(InferenceOverride {
                    model: Some("claude-haiku".to_string()),
                    ..Default::default()
                }),
                system_prompt: Some("You are in fast mode.".to_string()),
                ..Default::default()
            },
        );
        overlays.insert(
            "readonly".to_string(),
            AgentOverlay {
                allowed_tools: Some(vec![
                    "Read".to_string(),
                    "Glob".to_string(),
                    "Grep".to_string(),
                ]),
                excluded_tools: Some(vec!["Bash".to_string()]),
                ..Default::default()
            },
        );
        HandoffPlugin::new(overlays)
    }

    #[tokio::test]
    async fn no_handoff_returns_empty() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn same_agent_no_activate_action() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_handoff": {
                "active_agent": "fast",
                "requested_agent": "fast"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let has_state = actions
            .into_iter()
            .any(|a| matches!(a, BeforeInferenceAction::State(_)));
        assert!(!has_state);
    }

    #[tokio::test]
    async fn pending_handoff_emits_activate_and_overlay() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_handoff": {
                "active_agent": null,
                "requested_agent": "fast"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions: Vec<_> = AgentBehavior::before_inference(&p, &ctx)
            .await
            .into_iter()
            .collect();

        let has_state = actions
            .iter()
            .any(|a| matches!(a, BeforeInferenceAction::State(_)));
        let has_override = actions.iter().any(
            |a| matches!(a, BeforeInferenceAction::OverrideInference(ovr) if ovr.model.as_deref() == Some("claude-haiku")),
        );
        let has_system = actions.iter().any(
            |a| matches!(a, BeforeInferenceAction::AddSystemContext(s) if s.contains("fast mode")),
        );
        assert!(has_state, "should emit Activate state action");
        assert!(has_override, "should emit OverrideInference");
        assert!(has_system, "should emit AddSystemContext");
    }

    #[tokio::test]
    async fn active_agent_emits_overlay_without_activate() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_handoff": {
                "active_agent": "fast",
                "requested_agent": null
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions: Vec<_> = AgentBehavior::before_inference(&p, &ctx)
            .await
            .into_iter()
            .collect();

        let has_state = actions
            .iter()
            .any(|a| matches!(a, BeforeInferenceAction::State(_)));
        let has_override = actions
            .iter()
            .any(|a| matches!(a, BeforeInferenceAction::OverrideInference(_)));
        assert!(!has_state, "should NOT emit Activate (already active)");
        assert!(has_override, "should still emit OverrideInference");
    }

    #[tokio::test]
    async fn readonly_agent_blocks_write_tools() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_handoff": {
                "active_agent": "readonly"
            }
        }));

        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("Bash", "call_1", None);
        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(!actions.is_empty(), "Bash should be blocked");

        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("Read", "call_2", None);
        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(actions.is_empty(), "Read should be allowed");

        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("agent_handoff", "call_3", None);
        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(actions.is_empty(), "agent_handoff should always be allowed");
    }

    #[tokio::test]
    async fn no_active_agent_allows_all_tools() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("Bash", "call_1", None);

        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn unknown_agent_returns_empty() {
        let p = test_plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_handoff": {
                "active_agent": "nonexistent"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert!(actions.is_empty());
    }
}
