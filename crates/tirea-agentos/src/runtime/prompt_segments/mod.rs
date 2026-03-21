mod plugin;

#[cfg(test)]
mod tests;

pub use plugin::PromptSegmentsPlugin;

/// Behavior ID for the core prompt-segment bridge.
pub const PROMPT_SEGMENTS_PLUGIN_ID: &str = "prompt_segments";
