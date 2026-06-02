//! Agent runtime engine for the awaken framework.
//!
//! Implements the execution loop, phase pipeline, plugin system, state store,
//! and agent registry. Extension crates hook into this crate via the [`phase`],
//! [`plugins`], and [`extensions`] traits. Most users interact with this crate
//! indirectly through the `awaken` facade and `awaken::prelude`.

#![allow(missing_docs)]

pub mod agent;
pub mod backend;
pub mod builder;
pub(crate) mod cancellation;
pub mod checkpoint_store;
pub mod child_agent;
pub mod context;
pub mod credentials;
pub mod engine;
mod error;
pub mod event_buffer;
pub mod execution;
pub mod extensions;
mod hooks;
pub mod inbox;
pub mod loop_runner;
pub mod phase;
pub mod plugins;
pub mod policies;
pub mod profile;
pub mod registry;
pub mod resolution;
pub mod retry;
pub mod run;
pub mod runtime;
pub mod state;

// ── Core re-exports: types used directly by extension crates ──

// CancellationToken now lives in awaken-contract; re-export for backward compat.
pub use awaken_runtime_contract::{CancellationHandle, CancellationToken};
pub use checkpoint_store::RuntimeCheckpointStore;
pub use error::RuntimeError;
pub use event_buffer::EventBuffer;
pub use profile::ProfileAccess;

pub use backend::{
    BackendAbortRequest, BackendCancellationCapability, BackendContinuationCapability,
    BackendControl, BackendDelegateContinuation, BackendDelegatePersistence, BackendDelegatePolicy,
    BackendDelegateRunRequest, BackendLocalRootContext, BackendOutputArtifact,
    BackendOutputCapability, BackendParentContext, BackendRootRunRequest, BackendRunOutput,
    BackendRunResult, BackendRunStatus, BackendTranscriptCapability, BackendWaitCapability,
    ExecutionBackend, ExecutionBackendConfigSchema, ExecutionBackendError, ExecutionBackendFactory,
    ExecutionBackendFactoryError, LocalBackend, awaken_backend_config_schema,
};
pub use builder::{AgentRuntimeBuilder, BuildError};
pub use child_agent::{
    ChildAgentError, ChildAgentParams, ChildErrorForwarding, StreamingPassthroughSink,
    run_child_agent, run_child_agent_checked,
};
pub use phase::{
    DEFAULT_MAX_PHASE_ROUNDS, ExecutionEnv, PhaseContext, PhaseHook, PhaseRuntime, ToolGateHook,
    ToolPolicyHook, TypedEffectHandler, TypedScheduledActionHandler,
};
pub use plugins::{Plugin, PluginDescriptor, PluginRegistrar};
pub use registry::{
    AgentResolver, ProviderRemovalImpact, ProviderRemovalPolicy, ProviderRemovalPreview,
    RegistryDiagnostic, RegistryDiagnosticSeverity, RegistryResourceRef, RegistryUpdateError,
    RegistryValidationError, ResolvedAgent, RuntimeRegistryUpdate, SerializableRegistryDiagnostic,
    diagnose_agent_spec, diagnose_registry_set, diagnose_registry_set_serializable,
    preview_provider_removal, rebuild_agent_model_provider_registries,
};
pub use resolution::{
    AgentSpecLookup, BackendProfile, BackendRequirements, CapabilityDecision, CapabilityMismatch,
    DecisionCapability, DelegatePersistence, ExecutionPlan, ExecutionRole, FrontendToolCapability,
    HandoffTranscriptRef, LiveOnlyScope, LocalRegistryResolver, OverrideCapability,
    PersistenceCapability, PersistenceRequirement, RegistryResolutionScope, ReplayableResolvedRun,
    ReplayableScope, ResolutionArtifact, ResolutionPolicy, ResolutionRequest, ResolutionTarget,
    ResolveError, ResolvedModelBinding, ResolvedRun, ResolvedRunPlan, ResolvedTool, Resolver,
    RunFeatureSet,
};
pub use run::{
    CaptureWiring, PersistenceHints, ResolverInheritance, RunActivation, RunActivationError,
    RunControl, ThreadContextSnapshot,
};
pub use runtime::AgentRuntime;
pub use state::{CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore};
