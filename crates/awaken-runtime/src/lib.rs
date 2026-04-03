//! Agent runtime engine for the awaken framework.
//!
//! Implements the execution loop, phase pipeline, plugin system, state store,
//! and agent registry. Extension crates hook into this crate via the [`phase`],
//! [`plugins`], and [`extensions`] traits. Most users interact with this crate
//! indirectly through the [`awaken`] facade and [`awaken::prelude`].

#![allow(missing_docs)]

pub mod agent;
pub mod builder;
pub(crate) mod cancellation;
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
pub mod profile;
pub mod registry;
pub mod runtime;
pub mod state;

// ── Core re-exports: types used directly by extension crates ──

// CancellationToken now lives in awaken-contract; re-export for backward compat.
pub use awaken_contract::{CancellationHandle, CancellationToken};
pub use error::RuntimeError;
pub use profile::ProfileAccess;

pub use builder::{AgentRuntimeBuilder, BuildError};
pub use phase::{
    DEFAULT_MAX_PHASE_ROUNDS, ExecutionEnv, PhaseContext, PhaseHook, PhaseRuntime,
    TypedEffectHandler, TypedScheduledActionHandler,
};
pub use plugins::{Plugin, PluginDescriptor, PluginRegistrar};
pub use registry::{AgentResolver, ResolvedAgent};
pub use runtime::{AgentRuntime, RunRequest};
pub use state::{CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore};
