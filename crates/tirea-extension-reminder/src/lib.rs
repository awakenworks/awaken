//! Reminder policy extension.
//!
//! External callers only depend on [`ReminderAction`]. Internal reminder
//! state/reducer details are handled by [`ReminderPlugin`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::TypeId;
use std::collections::HashSet;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::state_spec::{
    reduce_state_actions, AnyStateAction, StateSpec,
};
use tirea_contract::runtime::plugin::phase::{AnyPluginAction, Phase};
use tirea_state::{State, TrackedPatch};

mod system_reminder;
pub use system_reminder::SystemReminder;

/// Reminder state stored in session state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "reminders")]
struct ReminderState {
    /// List of reminder texts.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Action type for `ReminderState` reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReminderAction {
    /// Add one reminder item.
    Add { text: String },
    /// Remove one reminder item.
    Remove { text: String },
    /// Clear all reminder items.
    Clear,
}

/// Stable plugin id for reminder actions.
pub const REMINDER_PLUGIN_ID: &str = "reminder";

impl From<ReminderAction> for AnyPluginAction {
    fn from(action: ReminderAction) -> Self {
        AnyPluginAction::new(REMINDER_PLUGIN_ID, action)
    }
}

impl StateSpec for ReminderState {
    type Action = ReminderAction;

    fn reduce(&mut self, action: ReminderAction) {
        match action {
            ReminderAction::Add { text } => self.items.push(text),
            ReminderAction::Remove { text } => self.items.retain(|item| item != &text),
            ReminderAction::Clear => self.items.clear(),
        }
    }
}

/// Plugin that manages system reminders.
///
/// This plugin:
/// - Initializes the reminder state
/// - Can clear reminders after they're used in `before_llm_request`
///
/// Note: The actual injection of reminders into LLM context is done by
/// the agent loop, which reads `ctx.reminders()` and formats them
/// appropriately.
pub struct ReminderPlugin {
    /// Whether to clear reminders after each LLM request.
    pub clear_after_llm_request: bool,
}

impl Default for ReminderPlugin {
    fn default() -> Self {
        Self {
            clear_after_llm_request: true,
        }
    }
}

impl ReminderPlugin {
    /// Create a new reminder plugin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to clear reminders after each LLM request.
    pub fn with_clear_after_llm_request(mut self, clear: bool) -> Self {
        self.clear_after_llm_request = clear;
        self
    }
}

#[async_trait]
impl AgentBehavior for ReminderPlugin {
    fn id(&self) -> &str {
        REMINDER_PLUGIN_ID
    }

    fn owned_states(&self) -> HashSet<TypeId> {
        HashSet::from([TypeId::of::<ReminderState>()])
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let reminders = ctx
            .snapshot_of::<ReminderState>()
            .ok()
            .map(|s| s.items)
            .unwrap_or_default();
        if reminders.is_empty() {
            return PhaseOutput::default();
        }

        let mut output = PhaseOutput::new();
        for text in &reminders {
            output = output.session_context(format!("Reminder: {}", text));
        }

        output
    }

    async fn phase_actions(&self, phase: Phase, ctx: &ReadOnlyContext<'_>) -> Vec<AnyPluginAction> {
        if phase != Phase::BeforeInference || !self.clear_after_llm_request {
            return Vec::new();
        }

        let has_reminders = ctx
            .snapshot_of::<ReminderState>()
            .ok()
            .map(|state| !state.items.is_empty())
            .unwrap_or(false);
        if !has_reminders {
            return Vec::new();
        }

        vec![ReminderAction::Clear.into()]
    }

    fn reduce_plugin_actions(
        &self,
        actions: Vec<AnyPluginAction>,
        base_snapshot: &serde_json::Value,
    ) -> Result<Vec<TrackedPatch>, String> {
        let mut state_actions = Vec::new();
        for action in actions {
            if action.plugin_id() != REMINDER_PLUGIN_ID {
                return Err(format!(
                    "reminder plugin received action for unexpected plugin '{}'",
                    action.plugin_id()
                ));
            }
            let action = action.downcast::<ReminderAction>().map_err(|other| {
                format!(
                    "reminder plugin failed to downcast action '{}'",
                    other.action_type_name()
                )
            })?;
            state_actions.push(AnyStateAction::new::<ReminderState>(action));
        }

        reduce_state_actions(state_actions, base_snapshot, "plugin:reminder")
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::plugin::phase::effect::PhaseEffect;
    use tirea_contract::runtime::plugin::phase::Phase;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    fn extract_session_contexts(output: &PhaseOutput) -> Vec<&str> {
        output
            .effects
            .iter()
            .filter_map(|e| match e {
                PhaseEffect::SessionContext(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_reminder_state_default() {
        let state = ReminderState::default();
        assert!(state.items.is_empty());
    }

    #[test]
    fn test_reminder_state_serialization() {
        let mut state = ReminderState::default();
        state.items.push("Reminder 1".to_string());
        state.items.push("Reminder 2".to_string());

        let json = serde_json::to_string(&state).unwrap();
        let parsed: ReminderState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.items.len(), 2);
    }

    #[test]
    fn test_reminder_plugin_reduces_actions() {
        let base = json!({
            "reminders": { "items": ["Keep", "Remove"] }
        });
        let actions = vec![
            ReminderAction::Add {
                text: "Added".to_string(),
            }
            .into(),
            ReminderAction::Remove {
                text: "Remove".to_string(),
            }
            .into(),
        ];
        let patches = ReminderPlugin::new()
            .reduce_plugin_actions(actions, &base)
            .expect("reminder action reduce should succeed");
        let next = tirea_state::apply_patches(&base, patches.iter().map(|p| p.patch()))
            .expect("patches should apply");
        assert_eq!(
            next["reminders"]["items"],
            json!(["Keep", "Added"]),
            "plugin actions should be reduced by reminder plugin"
        );
    }

    #[test]
    fn test_reminder_plugin_id() {
        let plugin = ReminderPlugin::new();
        assert_eq!(AgentBehavior::id(&plugin), REMINDER_PLUGIN_ID);
    }

    #[test]
    fn test_reminder_plugin_builder() {
        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        assert!(!plugin.clear_after_llm_request);
    }

    #[tokio::test]
    async fn test_reminder_plugin_before_inference() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Test reminder"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;
        let contexts = extract_session_contexts(&output);
        assert!(!contexts.is_empty());
        assert!(contexts[0].contains("Test reminder"));
    }

    #[tokio::test]
    async fn test_reminder_plugin_generates_clear_action() {
        let plugin = ReminderPlugin::new(); // clear_after_llm_request = true
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder A", "Reminder B"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;
        let actions = AgentBehavior::phase_actions(&plugin, Phase::BeforeInference, &ctx).await;
        let contexts = extract_session_contexts(&output);
        assert_eq!(contexts.len(), 2);
        assert!(contexts[0].contains("Reminder A"));
        assert!(contexts[1].contains("Reminder B"));
        assert_eq!(actions.len(), 1);
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_clear_when_disabled() {
        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;
        let actions = AgentBehavior::phase_actions(&plugin, Phase::BeforeInference, &ctx).await;
        let contexts = extract_session_contexts(&output);
        assert!(!contexts.is_empty());
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_empty_reminders() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": [] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_state() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(output.is_empty());
    }
}
