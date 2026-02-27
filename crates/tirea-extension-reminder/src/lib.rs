//! Reminder management extension for Context.
//!
//! Provides methods for managing system reminders that can be injected
//! into LLM context:
//! - `add_reminder(text)` - Add a reminder
//! - `reminders()` - Get all reminders
//!
//! The `ReminderPlugin` can inject reminders into the LLM request.
//!
//! # Example
//!
//! ```ignore
//! use tirea::prelude::*;
//!
//! async fn after_tool_execute(&self, ctx: &ContextAgentState, tool_id: &str, result: &ToolResult) {
//!     if tool_id == "file_read" {
//!         ctx.add_reminder("Remember to close the file when done");
//!     }
//! }
//! ```

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::state_spec::{AnyStateAction, StateSpec};
use tirea_contract::runtime::plugin::phase::{BeforeInferenceContext, PluginPhaseContext};
use tirea_contract::runtime::plugin::AgentPlugin;
use tirea_contract::runtime::tool_call::ToolCallContext;
use tirea_state::State;

mod system_reminder;
pub use system_reminder::SystemReminder;

/// Reminder state stored in session state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "reminders")]
pub struct ReminderState {
    /// List of reminder texts.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Action type for `ReminderState` reducer.
pub enum ReminderAction {
    /// Clear all reminder items.
    Clear,
}

impl StateSpec for ReminderState {
    type Action = ReminderAction;

    fn reduce(&mut self, action: ReminderAction) {
        match action {
            ReminderAction::Clear => self.items.clear(),
        }
    }
}

/// Extension trait for reminder management on Context.
pub trait ReminderContextExt {
    /// Add a reminder.
    fn add_reminder(&self, text: impl Into<String>);

    /// Get all reminders.
    fn reminders(&self) -> Vec<String>;

    /// Get the number of reminders.
    fn reminder_count(&self) -> usize;

    /// Clear all reminders.
    fn clear_reminders(&self);

    /// Remove a specific reminder by text.
    fn remove_reminder(&self, text: &str);
}

impl ReminderContextExt for ToolCallContext<'_> {
    fn add_reminder(&self, text: impl Into<String>) {
        let state = self.state_of::<ReminderState>();
        let _ = state.items_push(text.into());
    }

    fn reminders(&self) -> Vec<String> {
        let state = self.state_of::<ReminderState>();
        state.items().ok().unwrap_or_default()
    }

    fn reminder_count(&self) -> usize {
        self.reminders().len()
    }

    fn clear_reminders(&self) {
        let state = self.state_of::<ReminderState>();
        let _ = state.set_items(Vec::new());
    }

    fn remove_reminder(&self, text: &str) {
        let reminders = self.reminders();
        let filtered: Vec<String> = reminders.into_iter().filter(|r| r != text).collect();
        let state = self.state_of::<ReminderState>();
        let _ = state.set_items(filtered);
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
impl AgentPlugin for ReminderPlugin {
    fn id(&self) -> &str {
        "reminder"
    }

    async fn before_inference(&self, ctx: &mut BeforeInferenceContext<'_, '_>) {
        let reminders = ctx
            .state_of::<ReminderState>()
            .items()
            .ok()
            .unwrap_or_default();
        if reminders.is_empty() {
            return;
        }

        for text in &reminders {
            ctx.add_session_message(format!("Reminder: {}", text));
        }

        if self.clear_after_llm_request {
            let state = ctx.state_of::<ReminderState>();
            let _ = state.set_items(Vec::new());
        }
    }
}

#[async_trait]
impl AgentBehavior for ReminderPlugin {
    fn id(&self) -> &str {
        "reminder"
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

        if self.clear_after_llm_request {
            output = output
                .with_state_action(AnyStateAction::new::<ReminderState>(ReminderAction::Clear));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::plugin::phase::BeforeInferenceContext;
    use tirea_contract::testing::TestFixture;

    async fn run_before_inference(
        plugin: &ReminderPlugin,
        step: &mut tirea_contract::runtime::plugin::phase::StepContext<'_>,
    ) {
        let mut ctx = BeforeInferenceContext::new(step);
        AgentPlugin::before_inference(plugin, &mut ctx).await;
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
    fn test_add_reminder() {
        let fixture = TestFixture::new_with_state(json!({
            "reminders": { "items": [] }
        }));
        let ctx = fixture.ctx();

        ctx.state_of::<ReminderState>()
            .items_push("Test reminder".to_string())
            .expect("failed to append reminders.items");
        assert!(fixture.has_changes());
    }

    #[test]
    fn test_reminders_empty() {
        let fixture = TestFixture::new_with_state(json!({
            "reminders": { "items": [] }
        }));
        let ctx = fixture.ctx();

        let items = ctx
            .state_of::<ReminderState>()
            .items()
            .ok()
            .unwrap_or_default();
        assert!(items.is_empty());
    }

    #[test]
    fn test_reminders_with_existing() {
        let fixture = TestFixture::new_with_state(json!({
            "reminders": { "items": ["Reminder 1", "Reminder 2"] }
        }));
        let ctx = fixture.ctx();

        let items = ctx
            .state_of::<ReminderState>()
            .items()
            .ok()
            .unwrap_or_default();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_clear_reminders() {
        let fixture = TestFixture::new_with_state(json!({
            "reminders": { "items": ["Reminder 1", "Reminder 2"] }
        }));
        let ctx = fixture.ctx();

        let items = ctx
            .state_of::<ReminderState>()
            .items()
            .ok()
            .unwrap_or_default();
        assert_eq!(items.len(), 2);
        ctx.state_of::<ReminderState>()
            .set_items(Vec::new())
            .expect("failed to clear reminders.items");
        assert!(fixture.has_changes());
    }

    #[test]
    fn test_remove_reminder() {
        let fixture = TestFixture::new_with_state(json!({
            "reminders": { "items": ["Keep", "Remove", "Keep2"] }
        }));
        let ctx = fixture.ctx();

        let reminders: Vec<String> = ctx
            .state_of::<ReminderState>()
            .items()
            .ok()
            .unwrap_or_default();
        let filtered: Vec<String> = reminders.into_iter().filter(|r| r != "Remove").collect();
        ctx.state_of::<ReminderState>()
            .set_items(filtered)
            .expect("failed to update reminders.items");
        assert!(fixture.has_changes());
    }

    #[test]
    fn test_reminder_plugin_id() {
        let plugin = ReminderPlugin::new();
        assert_eq!(AgentPlugin::id(&plugin), "reminder");
    }

    #[test]
    fn test_reminder_plugin_builder() {
        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        assert!(!plugin.clear_after_llm_request);
    }

    #[tokio::test]
    async fn test_reminder_plugin_before_inference() {
        let fixture =
            TestFixture::new_with_state(json!({ "reminders": { "items": ["Test reminder"] } }));

        let plugin = ReminderPlugin::new();
        let mut step = fixture.step(vec![]);

        run_before_inference(&plugin, &mut step).await;

        assert!(!step.session_context.is_empty());
        assert!(step.session_context[0].contains("Test reminder"));
    }

    #[tokio::test]
    async fn test_reminder_plugin_generates_clear_patch() {
        let fixture = TestFixture::new_with_state(
            json!({ "reminders": { "items": ["Reminder A", "Reminder B"] } }),
        );

        let plugin = ReminderPlugin::new(); // clear_after_llm_request = true
        let mut step = fixture.step(vec![]);

        run_before_inference(&plugin, &mut step).await;

        // Should have injected reminders as session context
        assert_eq!(step.session_context.len(), 2);
        assert!(step.session_context[0].contains("Reminder A"));
        assert!(step.session_context[1].contains("Reminder B"));

        // Plugin ops are collected in fixture; verify changes were made
        assert!(fixture.has_changes());
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_clear_when_disabled() {
        let fixture =
            TestFixture::new_with_state(json!({ "reminders": { "items": ["Reminder"] } }));

        let plugin = ReminderPlugin::new().with_clear_after_llm_request(false);
        let mut step = fixture.step(vec![]);

        run_before_inference(&plugin, &mut step).await;

        assert!(!step.session_context.is_empty());
        // No pending patches when clearing is disabled
        assert!(step.pending_patches.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_empty_reminders() {
        let fixture = TestFixture::new_with_state(json!({ "reminders": { "items": [] } }));

        let plugin = ReminderPlugin::new();
        let mut step = fixture.step(vec![]);

        run_before_inference(&plugin, &mut step).await;

        assert!(step.session_context.is_empty());
        assert!(step.pending_patches.is_empty());
    }

    #[tokio::test]
    async fn test_reminder_plugin_no_state() {
        let fixture = TestFixture::new();

        let plugin = ReminderPlugin::new();
        let mut step = fixture.step(vec![]);

        run_before_inference(&plugin, &mut step).await;

        assert!(step.session_context.is_empty());
        assert!(step.pending_patches.is_empty());
    }
}
