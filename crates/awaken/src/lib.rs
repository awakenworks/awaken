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
//! use awaken::engine::GenaiExecutor;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = AgentRuntimeBuilder::new()
//!     .with_agent_spec(AgentSpec::new("assistant").with_model_id("gpt-4o-mini"))
//!     .with_provider("openai", Arc::new(GenaiExecutor::new()))
//!     .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
//!     .build()?;
//!
//! let activation = RunActivation::new("thread-1", vec![Message::user("Hello")])
//!     .with_agent_id("assistant");
//!
//! let result = runtime.run_to_completion(activation).await?;
//! let response = result.response;
//! # let _ = response;
//! # Ok(())
//! # }
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

/// Storage backend implementations (in-memory, file, PostgreSQL, SQLite mailbox).
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
pub use awaken_runtime_contract::contract;

/// Data-model primitives: phases, effects, scheduled actions.
pub use awaken_runtime_contract::model;

/// Agent-registry specification types.
pub use awaken_runtime_contract::registry_spec;

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

/// [`AgentRuntime`] and [`RunActivation`] — the top-level run API.
pub use awaken_runtime::runtime;

/// Agent configuration and instance types.
pub use awaken_runtime::agent;

/// Combined state types from both the contract and runtime layers.
pub mod state {
    pub use awaken_runtime::state::{
        CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore,
    };
    pub use awaken_runtime_contract::state::*;
}

// ── Flat re-exports: most commonly used types at crate root ──

// contract types
pub use awaken_runtime_contract::{
    AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY, AGENT_SPEC_UNKNOWN_FIELD_POLICY, AgentSpec,
    AgentSpecPatch, ConfigRecord, ConfigRecordError, ConfigRecordMerge, ConfigValidationError,
    EffectSpec, FailedScheduledActions, JsonValue, KeyScope, MODEL_SPEC_UNKNOWN_FIELD_POLICY,
    MergeStrategy, Modalities, Modality, ModelSpec, NoConfigPatch,
    PROVIDER_SPEC_UNKNOWN_FIELD_POLICY, PendingScheduledActions, PersistedState, Phase,
    PluginConfigKey, PreparedSkillSpecs, RecordMeta, RecordSource, RedactedString,
    SKILL_SPEC_UNKNOWN_FIELD_POLICY, ScheduledActionSpec, SkillArgumentSpec, SkillSpec,
    SkillSpecContext, SkillSpecPatch, SkillSpecSink, Snapshot, StateError, StateKey,
    StateKeyOptions, StateMap, TypedEffect, TypedTool, UnknownFieldPolicy, UnknownKeyPolicy,
    decode_config_record, effective_config_record, effective_visible_config_records,
    generate_tool_schema, merge_agent_spec, merge_skill_spec, sanitize_for_llm,
    validate_against_schema, validate_agent_spec, validate_agent_spec_patch,
    validate_config_record, validate_config_record_overrides, validate_model_spec,
    validate_provider_spec, validate_skill_spec, validate_unique_model_ids,
};
pub use awaken_server_contract::{
    ConfigStore, DEFAULT_SCOPE_ID, RequestSurface, ScopeContext, ScopeId, ScopedConfigStore,
    ScopedMailboxStore, ScopedOutboxStore, ScopedProtocolReplayLog, ScopedThreadRunStore,
    ScopedVersionedRegistry, scoped_key, unscoped_key,
};
/// Server/store-owned contract surfaces relocated out of `contract::*`.
pub mod server_contract {
    pub use awaken_server_contract::contract::config_store;
    pub use awaken_server_contract::contract::registry_graph;
    /// Full thread/run store traits + checkpoint adapter. The data types stay
    /// in `crate::contract::storage`; the `ThreadStore`/`RunStore`/
    /// `ThreadRunStore` traits moved here (server/store concern).
    pub use awaken_server_contract::contract::storage;
    pub use awaken_server_contract::contract::versioned_registry;
}

// runtime types
pub use awaken_runtime::engine::MockProviderProfile;
pub use awaken_runtime::{
    AgentResolver, AgentRuntime, AgentRuntimeBuilder, BuildError, CancellationToken, CommitEvent,
    CommitHook, DEFAULT_MAX_PHASE_ROUNDS, ExecutionEnv, MutationBatch, PhaseContext, PhaseHook,
    PhaseRuntime, Plugin, PluginDescriptor, PluginRegistrar, ProviderRemovalImpact,
    ProviderRemovalPolicy, ProviderRemovalPreview, RegistryDiagnostic, RegistryDiagnosticSeverity,
    RegistryResourceRef, RegistryUpdateError, RegistryValidationError, ResolvedAgent,
    RunActivation, RuntimeError, RuntimeRegistryUpdate, SerializableRegistryDiagnostic,
    StateCommand, StateStore, ToolGateHook, TypedEffectHandler, TypedScheduledActionHandler,
    diagnose_agent_spec, diagnose_registry_set, diagnose_registry_set_serializable,
    preview_provider_removal, rebuild_agent_model_provider_registries,
};
