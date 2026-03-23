//! Context management: compaction, summarization, truncation, and plugin state.

pub mod compaction;
pub mod plugin;
pub mod summarizer;
pub mod transform;
pub mod truncation;

pub use compaction::{
    find_compaction_boundary, record_compaction_boundary, trim_to_compaction_boundary,
};
pub use plugin::{
    CONTEXT_COMPACTION_PLUGIN_ID, CompactionAction, CompactionBoundary, CompactionConfig,
    CompactionConfigKey, CompactionPlugin, CompactionState, CompactionStateKey,
};
pub use summarizer::{
    ContextSummarizer, DefaultSummarizer, MIN_COMPACTION_GAIN_TOKENS, extract_previous_summary,
    render_transcript,
};
pub use transform::{
    ARTIFACT_COMPACT_THRESHOLD_TOKENS, ARTIFACT_PREVIEW_MAX_CHARS, ARTIFACT_PREVIEW_MAX_LINES,
    ContextTransform, compact_artifact, compact_tool_results,
};
pub use truncation::{TruncationState, continuation_message, should_retry};
