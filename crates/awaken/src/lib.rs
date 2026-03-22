#![allow(missing_docs)]

// ── Crate-level re-exports for modular access ──
pub use awaken_stores as stores;

#[cfg(feature = "mcp")]
pub use awaken_ext_mcp as ext_mcp;
#[cfg(feature = "observability")]
pub use awaken_ext_observability as ext_observability;
#[cfg(feature = "permission")]
pub use awaken_ext_permission as ext_permission;
#[cfg(feature = "skills")]
pub use awaken_ext_skills as ext_skills;
#[cfg(feature = "server")]
pub use awaken_server as server;

// ── Sub-module re-exports (backward compatible) ──

pub use awaken_contract::contract;
pub use awaken_contract::model;
pub use awaken_contract::registry_spec;

pub use awaken_runtime::agent;
pub use awaken_runtime::builder;
pub use awaken_runtime::engine;
pub use awaken_runtime::extensions;
pub use awaken_runtime::plugins;
pub use awaken_runtime::registry;
pub use awaken_runtime::runtime;

// ── Flat re-exports ──

// Re-export contract crate
pub use awaken_contract::{
    // registry spec
    AgentSpec,
    // model
    EffectSpec,
    FailedScheduledActions,
    JsonValue,
    // state (contract-level)
    KeyScope,
    MergeStrategy,
    PendingScheduledActions,
    PersistedState,
    Phase,
    PluginConfigKey,
    ScheduledActionSpec,
    Snapshot,
    StateError,
    StateKey,
    StateKeyOptions,
    StateMap,
    TypedEffect,
    UnknownKeyPolicy,
};

// Re-export runtime crate
pub use awaken_runtime::{
    // runtime
    AgentResolver,
    AgentRuntime,
    AppRuntime,
    CancellationToken,
    // state (runtime-level)
    CommitEvent,
    CommitHook,
    DEFAULT_MAX_PHASE_ROUNDS,
    ExecutionEnv,
    MutationBatch,
    PhaseContext,
    PhaseHook,
    PhaseRuntime,
    // plugins
    Plugin,
    PluginDescriptor,
    PluginRegistrar,
    ResolvedAgent,
    RunHandle,
    RunRequest,
    StateCommand,
    StateStore,
    ToolPermission,
    ToolPermissionChecker,
    ToolPermissionResult,
    TypedEffectHandler,
    TypedScheduledActionHandler,
    aggregate_tool_permissions,
};

// Re-export handoff and A2UI extensions at top level
pub use awaken_runtime::{
    A2uiPlugin, A2uiRenderTool, ActiveAgentKey, AgentOverlay, HandoffAction, HandoffPlugin,
    HandoffState, validate_a2ui_messages,
};

/// Re-export combined state module. Use `awaken::state` for all state types.
pub mod state {
    pub use awaken_contract::state::*;
    pub use awaken_runtime::state::{
        CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore,
    };
}
