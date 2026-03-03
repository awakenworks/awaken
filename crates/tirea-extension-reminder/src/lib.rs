//! Reminder policy extension.
//!
//! External callers only depend on [`ReminderAction`], [`AddReminderItem`],
//! [`InjectReminders`], [`ClearReminderState`], and [`ReminderPlugin`].

mod actions;
mod state;
mod system_reminder;

pub use actions::{AddReminderItem, ClearReminderState, InjectReminders};
pub use state::ReminderAction;
pub use system_reminder::SystemReminder;

use async_trait::async_trait;
use tirea_contract::runtime::action::Action;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};

/// Stable plugin id for reminder actions.
pub const REMINDER_PLUGIN_ID: &str = "reminder";

/// Plugin that manages system reminders.
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
    pub fn new() -> Self {
        Self::default()
    }

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

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        let reminders = ctx
            .snapshot_of::<state::ReminderState>()
            .ok()
            .map(|s| s.items)
            .unwrap_or_default();
        if reminders.is_empty() {
            return vec![];
        }

        let texts: Vec<String> = reminders
            .iter()
            .map(|text| format!("Reminder: {}", text))
            .collect();

        let mut actions: Vec<Box<dyn Action>> = vec![Box::new(InjectReminders(texts))];

        if self.clear_after_llm_request {
            actions.push(Box::new(ClearReminderState));
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    fn count_session_contexts(actions: &[Box<dyn Action>]) -> usize {
        actions
            .iter()
            .filter(|a| a.label() == "add_session_context")
            .count()
    }

    fn has_emit_state_patch(actions: &[Box<dyn Action>]) -> bool {
        actions.iter().any(|a| a.label() == "emit_state_patch")
    }

    #[test]
    fn test_reminder_state_default() {
        let s = state::ReminderState::default();
        assert!(s.items.is_empty());
    }

    #[test]
    fn test_reminder_state_serialization() {
        let mut s = state::ReminderState::default();
        s.items.push("Reminder 1".to_string());
        s.items.push("Reminder 2".to_string());

        let json = serde_json::to_string(&s).unwrap();
        let parsed: state::ReminderState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.items.len(), 2);
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
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(count_session_contexts(&actions) > 0);
    }

    #[tokio::test]
    async fn test_reminder_plugin_generates_clear_action() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder A", "Reminder B"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert_eq!(count_session_contexts(&actions), 1);
        assert!(has_emit_state_patch(&actions));
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_clear_when_disabled() {
        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(count_session_contexts(&actions) > 0);
        assert!(!has_emit_state_patch(&actions));
    }

    #[tokio::test]
    async fn test_reminder_plugin_empty_reminders() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({ "reminders": { "items": [] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_state() {
        let plugin = ReminderPlugin::new();
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(actions.is_empty());
    }

    #[test]
    fn add_reminder_item_emits_state_action() {
        let action = AddReminderItem("check logs".into());
        assert_eq!(action.label(), "emit_state_action");
    }

    #[test]
    fn reminder_reducer_add() {
        let mut s = state::ReminderState::default();
        s.reduce(ReminderAction::Add { text: "a".into() });
        s.reduce(ReminderAction::Add { text: "b".into() });
        s.reduce(ReminderAction::Add { text: "a".into() });
        assert_eq!(s.items, vec!["a", "b"]);
    }

    #[test]
    fn reminder_reducer_remove() {
        let mut s = state::ReminderState {
            items: vec!["a".into(), "b".into(), "c".into()],
        };
        s.reduce(ReminderAction::Remove { text: "b".into() });
        assert_eq!(s.items, vec!["a", "c"]);
    }

    #[test]
    fn reminder_reducer_clear() {
        let mut s = state::ReminderState {
            items: vec!["a".into(), "b".into()],
        };
        s.reduce(ReminderAction::Clear);
        assert!(s.items.is_empty());
    }
}
