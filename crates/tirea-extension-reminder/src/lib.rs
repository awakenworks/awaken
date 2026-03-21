//! Reminder policy extension.
//!
//! External callers can use reminder convenience helpers while prompt
//! persistence is now backed by the core prompt-segment state model.

mod actions;
mod state;
mod system_reminder;

pub use actions::{
    add_persistent_reminder_action, add_reminder_action, clear_reminder_action, inject_reminders,
};
pub use state::ReminderAction;
pub use system_reminder::SystemReminder;

use actions::{clear_reminder_action as _clear_action, inject_reminders as _inject};
use async_trait::async_trait;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};

/// Stable plugin id for reminder actions.
pub const REMINDER_PLUGIN_ID: &str = "reminder";

/// Legacy compatibility plugin for reminder state stored under `reminders`.
///
/// New reminder actions should use the core prompt-segment state path and do
/// not require this plugin to be wired. This plugin remains to honor
/// previously-persisted reminder state and older integrations.
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

    tirea_contract::declare_plugin_states!(state::ReminderState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let reminders = ctx
            .snapshot_of::<state::ReminderState>()
            .ok()
            .map(|s| s.items)
            .unwrap_or_default();
        if reminders.is_empty() {
            return ActionSet::empty();
        }

        let mut actions: ActionSet<BeforeInferenceAction> = _inject(reminders);

        if self.clear_after_llm_request {
            actions = actions.and(BeforeInferenceAction::State(_clear_action()));
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::runtime::state::{reduce_state_actions, ScopeContext};
    use tirea_contract::RunPolicy;
    use tirea_state::{apply_patch, DocCell};

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
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Test reminder"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        let add_context_count = actions
            .into_iter()
            .filter(|a| matches!(a, BeforeInferenceAction::AddContextMessage(_)))
            .count();
        assert!(add_context_count > 0);
    }

    #[tokio::test]
    async fn test_reminder_plugin_generates_clear_action() {
        let plugin = ReminderPlugin::new();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder A", "Reminder B"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        let v = actions.into_vec();
        let session_count = v
            .iter()
            .filter(|a| matches!(a, BeforeInferenceAction::AddContextMessage(_)))
            .count();
        let has_state = v
            .iter()
            .any(|a| matches!(a, BeforeInferenceAction::State(_)));
        // One session-context prompt segment is emitted per reminder item.
        assert_eq!(session_count, 2);
        assert!(has_state);
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_clear_when_disabled() {
        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "reminders": { "items": ["Reminder"] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        let v = actions.into_vec();
        let add_session_count = v
            .iter()
            .filter(|a| matches!(a, BeforeInferenceAction::AddContextMessage(_)))
            .count();
        let has_state = v
            .iter()
            .any(|a| matches!(a, BeforeInferenceAction::State(_)));
        assert!(add_session_count > 0);
        assert!(!has_state);
    }

    #[tokio::test]
    async fn test_reminder_plugin_empty_reminders() {
        let plugin = ReminderPlugin::new();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "reminders": { "items": [] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_state() {
        let plugin = ReminderPlugin::new();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;
        assert!(actions.is_empty());
    }

    #[test]
    fn add_reminder_item_writes_ephemeral_prompt_segment_state() {
        let sa = add_reminder_action("check logs");
        let patch = reduce_state_actions(vec![sa], &json!({}), "test", &ScopeContext::run())
            .expect("state action should reduce");
        let next = apply_patch(&json!({}), patch[0].patch()).expect("patch should apply");

        let items = next["__prompt_segments"]["items"]
            .as_array()
            .expect("prompt segments array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["namespace"], "reminder");
        assert_eq!(items[0]["consume"], "after_emit");
        assert_eq!(items[0]["content"], "Reminder: check logs");
    }

    #[test]
    fn add_persistent_reminder_item_writes_persistent_prompt_segment_state() {
        let sa = add_persistent_reminder_action("stay focused");
        let patch = reduce_state_actions(vec![sa], &json!({}), "test", &ScopeContext::run())
            .expect("state action should reduce");
        let next = apply_patch(&json!({}), patch[0].patch()).expect("patch should apply");

        let items = next["__prompt_segments"]["items"]
            .as_array()
            .expect("prompt segments array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["consume"], "persistent");
    }

    #[test]
    fn clear_reminder_action_clears_prompt_segment_namespace() {
        let base = json!({
            "__prompt_segments": {
                "items": [
                    {
                        "namespace": "reminder",
                        "key": "a",
                        "content": "Reminder: check logs",
                        "cooldown_turns": 0,
                        "target": "session",
                        "consume": "after_emit"
                    },
                    {
                        "namespace": "skill",
                        "key": "b",
                        "content": "tail",
                        "cooldown_turns": 0,
                        "target": "suffix_system",
                        "consume": "persistent"
                    }
                ]
            }
        });
        let patch = reduce_state_actions(
            vec![clear_reminder_action()],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .expect("state action should reduce");
        let next = apply_patch(&base, patch[0].patch()).expect("patch should apply");

        let items = next["__prompt_segments"]["items"]
            .as_array()
            .expect("prompt segments array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["namespace"], "skill");
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
