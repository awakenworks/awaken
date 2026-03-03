use super::state::{ReminderAction, ReminderState};
use tirea_contract::runtime::action::Action;
use tirea_contract::runtime::inference::InferenceContext;
use tirea_contract::runtime::phase::step::StepContext;
use tirea_contract::runtime::phase::Phase;
use tirea_contract::runtime::state::AnyStateAction;

/// Add a reminder item via typed state action.
pub struct AddReminderItem(pub String);

impl Action for AddReminderItem {
    fn label(&self) -> &'static str {
        "emit_state_action"
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.emit_state_action(AnyStateAction::new::<ReminderState>(ReminderAction::Add {
            text: self.0,
        }));
    }
}

/// Inject reminder texts into session context.
pub struct InjectReminders(pub Vec<String>);

impl Action for InjectReminders {
    fn label(&self) -> &'static str {
        "add_session_context"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "InjectReminders is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        let inf = step.extensions.get_or_default::<InferenceContext>();
        inf.session_context.extend(self.0);
    }
}

/// Clear reminder state after injection.
pub struct ClearReminderState;

impl Action for ClearReminderState {
    fn label(&self) -> &'static str {
        "emit_state_patch"
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.emit_state_action(AnyStateAction::new::<ReminderState>(ReminderAction::Clear));
    }
}
