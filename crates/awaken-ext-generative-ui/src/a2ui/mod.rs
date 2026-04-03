//! A2UI (Agent-to-UI) extension for awaken.
//!
//! Provides a tool-based interface for agents to emit declarative UI messages.
//! The LLM calls `A2uiRenderTool` with A2UI v0.8 JSON payloads; the tool
//! validates the structure and returns a minimal confirmation while the
//! frontend reads the tool input to render the surface.
//!
//! # Protocol overview
//!
//! A2UI v0.8 defines four message types:
//! - `surfaceUpdate` — define/update the component tree
//! - `dataModelUpdate` — populate or change data values
//! - `beginRendering` — signal the root component for initial render
//! - `deleteSurface` — remove a surface

mod plugin;
mod tool;
mod types;
mod validation;

pub use plugin::{A2uiPlugin, A2uiPromptConfig, A2uiPromptConfigKey, DEFAULT_A2UI_CATALOG_ID};
pub use tool::A2uiRenderTool;
pub use types::{
    A2uiBeginRendering, A2uiComponent, A2uiDataModelEntry, A2uiDataModelUpdate, A2uiDeleteSurface,
    A2uiMessage, A2uiSurfaceUpdate,
};
pub use validation::{A2uiValidationError, validate_a2ui_messages};

pub(crate) const A2UI_TOOL_ID: &str = "render_a2ui";
pub(crate) const A2UI_TOOL_NAME: &str = "render_a2ui";
pub const A2UI_PLUGIN_ID: &str = "generative-ui";
pub(crate) const MESSAGE_KEYS: &[&str] = &[
    "surfaceUpdate",
    "dataModelUpdate",
    "beginRendering",
    "deleteSurface",
];

#[cfg(test)]
mod tests;
