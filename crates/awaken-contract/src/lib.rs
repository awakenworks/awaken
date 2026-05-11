//! Core contract types and traits for the awaken agent framework.
//!
//! This crate defines the shared vocabulary used across all `awaken-*` crates:
//! data-model primitives (phases, effects, state), protocol traits (tools,
//! inference, events, lifecycle, storage), and utility types (cancellation,
//! threads, time). Runtime crates implement these traits; user code and
//! extension crates consume them.
//!
//! Most items are re-exported from the `awaken` facade crate. Import
//! `awaken::prelude::*` for the common subset, or use the sub-modules below
//! for precise imports.

#![allow(missing_docs)]

pub mod agent_spec_patch;
pub mod builtin_seed;
pub mod cancellation;
pub mod config_loader;
pub mod config_record;
pub mod config_validation;
pub mod contract;
mod error;
pub mod identity;
pub mod model;
pub mod periodic_refresh;
pub mod registry_spec;
pub mod secret;
pub mod state;
pub mod thread;
pub mod time;
pub mod tool_spec;
pub mod tool_spec_patch;

// ── time ──
pub use time::now_ms;

// ── error ──
pub use error::{StateError, UnknownKeyPolicy};

// ── model ──
pub use model::{
    EffectSpec, FailedScheduledActions, JsonValue, PendingScheduledActions, Phase,
    ScheduledActionSpec, TypedEffect,
};

// ── agent spec patch ──
pub use agent_spec_patch::{AgentSpecPatch, merge_agent_spec};

// ── tool spec patch ──
pub use tool_spec_patch::{ToolSpecPatch, merge_tool_spec};

// ── registry spec (AgentSpec, PluginConfigKey) ──
pub use registry_spec::{
    AgentSpec, McpRestartPolicy, McpServerSpec, McpTransportKind, ModelBindingSpec,
    PluginConfigKey, ProviderSpec,
};

// ── secret ──
pub use secret::RedactedString;

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
    MailboxInterrupt, MailboxInterruptDetails, MailboxStore, RunDispatch, RunDispatchResult,
    RunDispatchStatus,
};
pub use contract::storage::RunRequestOrigin;

// ── profile store ──
pub use contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
};
pub use contract::profile_store::{ProfileEntry, ProfileKey, ProfileOwner, ProfileStore};

// ── shared state ──
pub use contract::shared_state::StateScope;

// ── tool schema ──
pub use contract::tool::TypedTool;
pub use contract::tool_schema::{generate_tool_schema, sanitize_for_llm, validate_against_schema};

// ── thread ──
pub use thread::{Thread, ThreadMetadata};

// ── tool spec ──
pub use tool_spec::ToolSpec;

// ── cancellation ──
pub use cancellation::{CancellationHandle, CancellationToken};

// ── periodic refresh ──
pub use periodic_refresh::PeriodicRefresher;

// ── audit log ──
pub use contract::audit_log::{AuditAction, AuditEvent};

// ── config record envelope ──
pub use config_record::{
    ConfigRecord, ConfigRecordError, ConfigRecordMerge, NoConfigPatch, RecordMeta, RecordSource,
    decode_config_record, effective_config_record, effective_visible_config_records,
    validate_config_record_overrides,
};
pub use config_validation::{
    AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY, AGENT_SPEC_UNKNOWN_FIELD_POLICY, ConfigValidationError,
    MODEL_BINDING_SPEC_UNKNOWN_FIELD_POLICY, PROVIDER_SPEC_UNKNOWN_FIELD_POLICY,
    UnknownFieldPolicy, validate_agent_spec, validate_agent_spec_patch, validate_config_record,
    validate_model_binding_spec, validate_provider_spec,
};

// ── builtin seed ──
pub use builtin_seed::{BuiltinSeedSet, BuiltinSpec};
