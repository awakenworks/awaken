mod plugin;
mod state;

#[cfg(test)]
mod tests;

pub use plugin::PromptSegmentsPlugin;
pub use state::{
    consume_after_emit_context_messages_action, remove_context_message_action,
    remove_context_messages_by_prefix_action, upsert_context_message_action, PromptSegmentAction,
    PromptSegmentState,
};

/// Behavior ID for the core prompt-segment bridge.
pub const PROMPT_SEGMENTS_PLUGIN_ID: &str = "prompt_segments";
