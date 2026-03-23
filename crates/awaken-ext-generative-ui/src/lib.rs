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
    A2uiComponent, A2uiCreateSurface, A2uiDeleteSurface, A2uiMessage, A2uiPlugin, A2uiRenderTool,
    A2uiUpdateComponents, A2uiUpdateDataModel, A2uiValidationError, validate_a2ui_messages,
};
