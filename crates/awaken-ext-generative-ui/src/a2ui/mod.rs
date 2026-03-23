//! A2UI (Agent-to-UI) extension for awaken.
//!
//! Provides a tool-based interface for agents to emit declarative UI messages.
//! The LLM calls `A2uiRenderTool` with A2UI JSON payloads; the tool validates
//! the structure and returns the validated payload as its result, which flows
//! through the event stream to the frontend.
//!
//! # Protocol overview
//!
//! A2UI v0.9 defines four message types:
//! - `createSurface` — initialize a rendering surface
//! - `updateComponents` — define/update the component tree
//! - `updateDataModel` — populate or change data values
//! - `deleteSurface` — remove a surface

mod plugin;
mod tool;
mod types;
mod validation;

pub use plugin::A2uiPlugin;
pub use tool::A2uiRenderTool;
pub use types::{
    A2uiComponent, A2uiCreateSurface, A2uiDeleteSurface, A2uiMessage, A2uiUpdateComponents,
    A2uiUpdateDataModel,
};
pub use validation::{A2uiValidationError, validate_a2ui_messages};

pub(crate) const A2UI_TOOL_ID: &str = "render_a2ui";
pub(crate) const A2UI_TOOL_NAME: &str = "render_a2ui";
pub(crate) const A2UI_PLUGIN_ID: &str = "a2ui";
pub(crate) const SUPPORTED_VERSION: &str = "v0.9";
pub(crate) const MESSAGE_KEYS: &[&str] = &[
    "createSurface",
    "updateComponents",
    "updateDataModel",
    "deleteSurface",
];

#[cfg(test)]
mod tests;
