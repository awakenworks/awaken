#![allow(missing_docs)]

pub mod config_loader;
pub mod contract;
mod error;
pub mod model;
pub mod registry_spec;
pub mod state;
pub mod thread;
pub mod time;

// ── time ──
pub use time::now_ms;

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

// ── progress ──
pub use contract::progress::{
    ProgressStatus, TOOL_CALL_PROGRESS_ACTIVITY_TYPE, ToolCallProgressState,
};

// ── mailbox ──
pub use contract::mailbox::{
    MailboxInterrupt, MailboxJob, MailboxJobOrigin, MailboxJobStatus, MailboxStore,
};

// ── profile store ──
pub use contract::profile_store::{ProfileEntry, ProfileKey, ProfileOwner, ProfileStore};

// ── tool schema ──
pub use contract::tool::TypedTool;
pub use contract::tool_schema::{generate_tool_schema, sanitize_for_llm, validate_against_schema};

// ── thread ──
pub use thread::{Thread, ThreadMetadata};
