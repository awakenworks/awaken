#![allow(missing_docs)]

pub mod a2ui;
mod run;
mod sink;

#[cfg(feature = "openui")]
pub mod openui;

#[cfg(feature = "json-render")]
pub mod json_render;

pub use run::{StreamingSubagentResult, run_streaming_subagent};
pub use sink::StreamingSubagentSink;

// Re-export a2ui public types
pub use a2ui::{
    A2UI_PLUGIN_ID, A2uiBeginRendering, A2uiComponent, A2uiDataModelEntry, A2uiDataModelUpdate,
    A2uiDeleteSurface, A2uiMessage, A2uiPlugin, A2uiPromptConfig, A2uiPromptConfigKey,
    A2uiRenderTool, A2uiSurfaceUpdate, A2uiValidationError, DEFAULT_A2UI_CATALOG_ID,
    validate_a2ui_messages,
};
