//! Context management: compaction, summarization, truncation, and plugin state.

pub mod compaction;
pub mod effective_policy;
pub mod plugin;
pub mod summarizer;
pub mod transform;
pub mod truncation;

pub use compaction::{
    AppliedCompaction, COMPACTION_COMPLETED_EVENT, COMPACTION_FAILED_EVENT, CompactionPlan,
    apply_summary, clear_compaction_in_flight, find_compaction_boundary, plan_compaction,
    record_compaction_boundary, record_compaction_failure, record_compaction_in_flight,
    trim_to_compaction_boundary, try_consume_compaction_event,
};
pub use effective_policy::effective_policy;
pub use plugin::{
    CONTEXT_COMPACTION_PLUGIN_ID, CONTEXT_TRANSFORM_PLUGIN_ID, CompactionAction,
    CompactionBoundary, CompactionConfig, CompactionConfigKey, CompactionFailure,
    CompactionInFlight, CompactionPlugin, CompactionState, CompactionStateKey,
    ContextTransformPlugin,
};
pub use summarizer::{
    ContextSummarizer, DefaultSummarizer, MIN_COMPACTION_GAIN_TOKENS, SummarizationError,
    extract_previous_summary, render_transcript,
};
pub use transform::{
    ARTIFACT_COMPACT_THRESHOLD_TOKENS, ARTIFACT_PREVIEW_MAX_CHARS, ARTIFACT_PREVIEW_MAX_LINES,
    ContextTransform, compact_artifact, compact_tool_results,
};
pub use truncation::{TruncationState, continuation_message, should_retry};
