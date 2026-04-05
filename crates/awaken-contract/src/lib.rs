//! Core contract types and traits for the awaken agent framework.
//!
//! This crate defines the shared vocabulary used across all `awaken-*` crates:
//! data-model primitives (phases, effects, state), protocol traits (tools,
//! inference, events, lifecycle, storage), and utility types (cancellation,
//! threads, time). Runtime crates implement these traits; user code and
//! extension crates consume them.
//!
//! Most items are re-exported from the [`awaken`] facade crate. Import
//! `awaken::prelude::*` for the common subset, or use the sub-modules below
//! for precise imports.

#![allow(missing_docs)]

pub mod cancellation;
pub mod config_loader;
pub mod contract;
mod error;
pub mod model;
pub mod periodic_refresh;
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

// ── A2A ──
pub use contract::a2a::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill};

// ── registry spec (AgentSpec, PluginConfigKey) ──
pub use registry_spec::{AgentSpec, PluginConfigKey};

// ── state ──
pub use state::{
    KeyScope, MergeStrategy, MutationBatch, StateCommand, StateKey, StateKeyOptions, StateMap,
};
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

// ── cancellation ──
pub use cancellation::{CancellationHandle, CancellationToken};

// ── periodic refresh ──
pub use periodic_refresh::PeriodicRefresher;
