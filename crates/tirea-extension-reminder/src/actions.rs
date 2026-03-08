use super::state::{ReminderAction, ReminderState};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea_contract::runtime::state::AnyStateAction;

/// Create a state action that adds a reminder item.
pub fn add_reminder_action(text: impl Into<String>) -> AnyStateAction {
    AnyStateAction::new::<ReminderState>(ReminderAction::Add { text: text.into() })
}

/// Inject reminder texts as session context entries.
pub fn inject_reminders(texts: Vec<String>) -> ActionSet<BeforeInferenceAction> {
    texts
        .into_iter()
        .map(BeforeInferenceAction::AddSessionContext)
        .collect::<Vec<_>>()
        .into()
}

/// Create a state action that clears reminder state.
pub fn clear_reminder_action() -> AnyStateAction {
    AnyStateAction::new::<ReminderState>(ReminderAction::Clear)
}
