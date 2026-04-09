//! The `awaken` facade crate — the primary entry point for building AI agents.
//!
//! This crate re-exports everything you need from the underlying `awaken-*` crates
//! so that user code only needs a single dependency. Start with [`prelude`] for a
//! one-import convenience layer, or access individual modules directly.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use awaken::prelude::*;
//!
//! let runtime = AgentRuntimeBuilder::new("my-agent")
//!     .build()
//!     .await?;
//!
//! runtime.run(RunRequest::default()).await?;
//! ```
//!
//! # Module layout
//!
//! | Path | Description |
//! |------|-------------|
//! | [`prelude`] | One-stop import for common agent-building types |
//! | [`contract`] | Core protocol traits: tools, inference, events, lifecycle |
//! | [`model`] | Data-model primitives: phases, effects, scheduled actions |
//! | [`state`] | State key/value types and mutation primitives |
//! | [`registry`] | Agent registry and resolution |
//! | [`builder`] | [`AgentRuntimeBuilder`] — fluent runtime construction |
//! | [`plugins`] | Plugin trait and descriptor types |
//! | [`phase`] | Phase execution context and hooks |
//! | [`engine`] | Low-level agent execution engine |
//! | [`stores`] | Storage backend implementations |
//! | [`server`] | HTTP server layer (feature `server`) |

pub mod prelude;

/// Storage backend implementations (in-memory, file, PostgreSQL).
pub use awaken_stores as stores;

/// Generative-UI extension (feature `generative-ui`).
#[cfg(feature = "generative-ui")]
pub use awaken_ext_generative_ui as ext_generative_ui;

/// MCP (Model Context Protocol) tool-bridge extension (feature `mcp`).
#[cfg(feature = "mcp")]
pub use awaken_ext_mcp as ext_mcp;

/// Observability / tracing extension (feature `observability`).
#[cfg(feature = "observability")]
pub use awaken_ext_observability as ext_observability;

/// Tool-permission / human-in-the-loop extension (feature `permission`).
#[cfg(feature = "permission")]
pub use awaken_ext_permission as ext_permission;

/// Reminder / periodic-context-injection extension (feature `reminder`).
#[cfg(feature = "reminder")]
pub use awaken_ext_reminder as ext_reminder;

/// Skills discovery and dispatch extension (feature `skills`).
#[cfg(feature = "skills")]
pub use awaken_ext_skills as ext_skills;

/// HTTP server layer (feature `server`).
#[cfg(feature = "server")]
pub use awaken_server as server;

// ── Sub-crate module re-exports ──

/// Core protocol traits: tools, inference, events, lifecycle, storage contracts.
pub use awaken_contract::contract;

/// Data-model primitives: phases, effects, scheduled actions.
pub use awaken_contract::model;

/// Agent-registry specification types.
pub use awaken_contract::registry_spec;

/// Fluent builder for constructing an [`AgentRuntime`].
pub use awaken_runtime::builder;

/// Execution context and request/response wrappers.
pub use awaken_runtime::context;

/// Low-level agent execution engine.
pub use awaken_runtime::engine;

/// Execution-environment helpers and run orchestration.
pub use awaken_runtime::execution;

/// Extension-point traits for integrating with the runtime.
pub use awaken_runtime::extensions;

/// Agent run-loop runner.
pub use awaken_runtime::loop_runner;

/// Phase execution context, hooks, and phase-level runtime.
pub use awaken_runtime::phase;

/// Plugin loading, descriptor registry, and registration API.
pub use awaken_runtime::plugins;

/// Stop policies and run-termination conditions.
pub use awaken_runtime::policies;

/// Agent registry lookup and resolution.
pub use awaken_runtime::registry;

/// [`AgentRuntime`] and [`RunRequest`] — the top-level run API.
pub use awaken_runtime::runtime;

/// Agent configuration and instance types.
pub use awaken_runtime::agent;

/// Combined state types from both the contract and runtime layers.
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
    RuntimeError, StateCommand, StateStore, ToolGateHook, TypedEffectHandler,
    TypedScheduledActionHandler,
};
