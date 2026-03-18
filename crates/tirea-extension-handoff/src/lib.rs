//! Agent handoff extension for tirea agents.
//!
//! Provides dynamic same-thread agent switching:
//!
//! 1. **`AgentHandoffTool`** (in `tirea-agentos`) writes a handoff request
//! 2. **[`HandoffPlugin`]** applies the target agent's overlay dynamically in
//!    `before_inference` and enforces tool restrictions in `before_tool_execute`
//!
//! No run termination or re-resolution occurs — handoff is instant.
//! State is thread-scoped and persists across runs on the same thread.

mod actions;
mod plugin;
mod state;

pub use actions::{activate_handoff_action, clear_handoff_action, request_handoff_action};
pub use plugin::{HandoffPlugin, HandoffRuntimeOverlay, HANDOFF_PLUGIN_ID};
pub use state::{HandoffAction, HandoffState};
