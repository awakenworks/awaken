#![allow(missing_docs)]

pub mod agent;
pub mod builder;
mod cancellation;
pub mod context;
pub mod engine;
mod error;
pub mod execution;
pub mod extensions;
mod hooks;
pub mod loop_runner;
pub mod phase;
pub mod plugins;
pub mod policies;
pub mod registry;
pub mod runtime;
pub mod state;

// ── Core re-exports: types used directly by extension crates ──

pub use cancellation::CancellationToken;
pub use error::RuntimeError;

pub use builder::{AgentRuntimeBuilder, BuildError};
pub use phase::{
    DEFAULT_MAX_PHASE_ROUNDS, ExecutionEnv, PhaseContext, PhaseHook, PhaseRuntime, ToolPermission,
    ToolPermissionChecker, ToolPermissionResult, TypedEffectHandler, TypedScheduledActionHandler,
    aggregate_tool_permissions,
};
pub use plugins::{AllowAllToolsPlugin, Plugin, PluginDescriptor, PluginRegistrar};
pub use registry::{AgentResolver, ResolvedAgent};
pub use runtime::{AgentRuntime, RunRequest};
pub use state::{CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore};
