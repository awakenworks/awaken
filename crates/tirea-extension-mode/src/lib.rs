//! Agent mode switching extension for tirea agents.
//!
//! Provides named configuration profiles (modes) with runtime switching:
//!
//! 1. **[`SwitchModeTool`]** allows the LLM to request a mode switch
//! 2. **[`ModePlugin`]** detects the pending switch and terminates the run
//! 3. **`AgentOs::run()`** re-resolves the agent with the new mode overlay
//!
//! Mode state is thread-scoped and persists across runs on the same thread.

mod actions;
mod plugin;
mod state;
mod tools;

pub use actions::{activate_mode_action, request_mode_switch_action};
pub use plugin::{ModePlugin, MODE_PLUGIN_ID, MODE_SWITCH_STOP_CODE};
pub use state::{AgentModeAction, AgentModeState};
pub use tools::SwitchModeTool;
