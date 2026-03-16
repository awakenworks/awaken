use super::state::{PlanModeAction, PlanModeState, PlanRef};
use tirea_contract::runtime::phase::BeforeToolExecuteAction;
use tirea_contract::runtime::state::AnyStateAction;

/// Create a state action that enters plan mode.
pub fn enter_plan_mode_action() -> AnyStateAction {
    AnyStateAction::new::<PlanModeState>(PlanModeAction::Enter)
}

/// Create a state action that exits plan mode with an approved plan.
pub fn exit_plan_mode_action(plan_ref: PlanRef) -> AnyStateAction {
    AnyStateAction::new::<PlanModeState>(PlanModeAction::Exit { plan_ref })
}

/// Create a state action that deactivates plan mode without a plan.
pub fn deactivate_plan_mode_action() -> AnyStateAction {
    AnyStateAction::new::<PlanModeState>(PlanModeAction::Deactivate)
}

/// Block a write tool during plan mode.
pub fn block_write_in_plan_mode(tool_id: &str) -> BeforeToolExecuteAction {
    BeforeToolExecuteAction::Block(format!(
        "Plan mode is active. Tool '{tool_id}' is blocked — only read-only tools are allowed."
    ))
}
