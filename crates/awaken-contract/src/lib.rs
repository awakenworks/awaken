#![allow(missing_docs)]

pub mod contract;
mod error;
pub mod model;
pub mod registry_spec;
pub mod state;
pub mod thread;

// ── error ──
pub use error::{StateError, UnknownKeyPolicy};

// ── model ──
pub use model::{
    EffectSpec, FailedScheduledActions, JsonValue, PendingScheduledActions, Phase,
    ScheduledActionSpec, TypedEffect,
};

// ── registry spec (AgentSpec, PluginConfigKey) ──
pub use registry_spec::{AgentSpec, PluginConfigKey};

// ── state ──
pub use state::{KeyScope, MergeStrategy, StateKey, StateKeyOptions, StateMap};
pub use state::{PersistedState, Snapshot};

// ── thread ──
pub use thread::{Thread, ThreadMetadata};
