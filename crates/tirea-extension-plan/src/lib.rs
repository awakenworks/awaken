//! Plan mode extension for tirea agents.
//!
//! Provides structured plan-before-execute workflow with two-layer enforcement:
//!
//! 1. **System prompt injection** instructs the model to use only read-only tools
//! 2. **Hard tool gate** blocks write tools at `before_tool_execute` even if the
//!    model ignores the prompt
//!
//! Plans are stored as [`PlanRef`] in the thread's state doc — no local
//! filesystem dependency. Any thread holding a `PlanRef` can reference the plan.
//! Sub-agents are blocked from entering plan mode.
//!
//! ## Usage
//!
//! Register [`PlanModePlugin`] as an `AgentBehavior` and add [`EnterPlanModeTool`]
//! and [`ExitPlanModeTool`] to the tool registry.

mod actions;
mod plugin;
mod prompt;
mod state;
mod tools;

pub use actions::{deactivate_plan_mode_action, enter_plan_mode_action, exit_plan_mode_action};
pub use plugin::{PlanModePlugin, PLAN_MODE_PLUGIN_ID};
pub use state::{PlanModeAction, PlanModeState, PlanRef};
pub use tools::{EnterPlanModeTool, ExitPlanModeTool};
