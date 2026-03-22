#![allow(missing_docs)]

pub mod contract;
mod error;
pub mod model;
pub mod registry_spec;
pub mod state;
pub mod thread;
pub mod tool_pattern;

// ── error ──
pub use error::{StateError, UnknownKeyPolicy};

// ── model ──
pub use model::{
    EffectSpec, FailedScheduledActions, JsonValue, PendingScheduledActions, Phase,
    ScheduledActionSpec, TypedEffect,
};

// ── agent card ──
pub use contract::agent_card::{AgentCard, AgentCardAuth};

// ── registry spec (AgentSpec, PluginConfigKey) ──
pub use registry_spec::{AgentSpec, PluginConfigKey};

// ── state ──
pub use state::{KeyScope, MergeStrategy, StateKey, StateKeyOptions, StateMap};
pub use state::{PersistedState, Snapshot};

// ── progress / file activity ──
pub use contract::progress::{
    FILE_ACTIVITY_TYPE, FileActivity, FileOperation, ProgressStatus,
    TOOL_CALL_PROGRESS_ACTIVITY_TYPE, ToolCallProgressState,
};

// ── thread ──
pub use thread::{Thread, ThreadMetadata};
