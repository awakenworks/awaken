//! Agent handoff extension — dynamic same-thread agent switching.
//!
//! Manages agent variant switching within a running agent loop:
//!
//! 1. `HandoffState` tracks active and requested agent variants.
//! 2. `HandoffPlugin` reads state and applies agent overlays dynamically.
//! 3. `AgentOverlay` defines per-variant overrides (system prompt, model, tools).
//!
//! No run termination or re-resolution occurs — handoff is instant.

mod action;
mod hook;
mod plugin;
mod state;
mod types;

pub use action::{HandoffAction, activate_handoff, clear_handoff, request_handoff};
pub use plugin::{HANDOFF_PLUGIN_ID, HandoffPlugin};
pub use state::{ActiveAgentKey, HandoffState};
pub use types::AgentOverlay;

#[cfg(test)]
mod tests;
