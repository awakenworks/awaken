use serde::{Deserialize, Serialize};
use tirea_state::State;

/// Reminder state stored in session state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "reminders", action = "ReminderAction")]
pub(super) struct ReminderState {
    #[serde(default)]
    pub items: Vec<String>,
}

/// Action type for `ReminderState` reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReminderAction {
    /// Add a reminder item (deduplicated).
    Add { text: String },
    /// Remove one reminder item.
    Remove { text: String },
    /// Clear all reminder items.
    Clear,
}

impl ReminderState {
    pub(super) fn reduce(&mut self, action: ReminderAction) {
        match action {
            ReminderAction::Add { text } => {
                if !self.items.contains(&text) {
                    self.items.push(text);
                }
            }
            ReminderAction::Remove { text } => self.items.retain(|item| item != &text),
            ReminderAction::Clear => self.items.clear(),
        }
    }
}
