#![allow(missing_docs)]

pub mod prelude;

// ── Crate-level re-exports for modular access ──
pub use awaken_stores as stores;

#[cfg(feature = "generative-ui")]
pub use awaken_ext_generative_ui as ext_generative_ui;
#[cfg(feature = "mcp")]
pub use awaken_ext_mcp as ext_mcp;
#[cfg(feature = "observability")]
pub use awaken_ext_observability as ext_observability;
#[cfg(feature = "permission")]
pub use awaken_ext_permission as ext_permission;
#[cfg(feature = "reminder")]
pub use awaken_ext_reminder as ext_reminder;
#[cfg(feature = "skills")]
pub use awaken_ext_skills as ext_skills;
#[cfg(feature = "server")]
pub use awaken_server as server;

// ── Sub-crate module re-exports ──

pub use awaken_contract::contract;
pub use awaken_contract::model;
pub use awaken_contract::registry_spec;

pub use awaken_runtime::builder;
pub use awaken_runtime::context;
pub use awaken_runtime::engine;
pub use awaken_runtime::execution;
pub use awaken_runtime::extensions;
pub use awaken_runtime::loop_runner;
pub use awaken_runtime::phase;
pub use awaken_runtime::plugins;
pub use awaken_runtime::policies;
pub use awaken_runtime::registry;
pub use awaken_runtime::runtime;

// ── Agent module: config + state from runtime ──
pub use awaken_runtime::agent;

// ── State: combined contract + runtime types ──
pub mod state {
    pub use awaken_contract::state::*;
    pub use awaken_runtime::state::{
        CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore,
    };
}

// ── Flat re-exports: most commonly used types at crate root ──

// contract types
pub use awaken_contract::{
    AgentSpec, EffectSpec, FailedScheduledActions, JsonValue, KeyScope, MergeStrategy,
    PendingScheduledActions, PersistedState, Phase, PluginConfigKey, ScheduledActionSpec, Snapshot,
    StateError, StateKey, StateKeyOptions, StateMap, TypedEffect, TypedTool, UnknownKeyPolicy,
    generate_tool_schema, sanitize_for_llm, validate_against_schema,
};

// runtime types
pub use awaken_runtime::{
    AgentResolver, AgentRuntime, AgentRuntimeBuilder, BuildError, CancellationToken, CommitEvent,
    CommitHook, DEFAULT_MAX_PHASE_ROUNDS, ExecutionEnv, MutationBatch, PhaseContext, PhaseHook,
    PhaseRuntime, Plugin, PluginDescriptor, PluginRegistrar, ResolvedAgent, RunRequest,
    RuntimeError, StateCommand, StateStore, TypedEffectHandler, TypedScheduledActionHandler,
};
