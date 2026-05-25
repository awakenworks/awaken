//! Context plugins: compaction state tracking and request-transform installation.

mod compaction;
mod context_transform;
mod knowledge_cutoff;

pub use compaction::{
    CONTEXT_COMPACTION_PLUGIN_ID, CompactionAction, CompactionBoundary, CompactionConfig,
    CompactionConfigKey, CompactionFailure, CompactionInFlight, CompactionPlugin,
    CompactionSkipped, CompactionState, CompactionStateKey,
};
pub use context_transform::{
    CONTEXT_TRANSFORM_PLUGIN_ID, ContextTransformConfig, ContextTransformConfigKey,
    ContextTransformPlugin,
};
pub use knowledge_cutoff::{KNOWLEDGE_CUTOFF_PLUGIN_ID, KnowledgeCutoffPlugin};
