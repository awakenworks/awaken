// @formal entrypoint session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence
//! `awaken-session-executor` — authority-free shared execution host infrastructure.
//!
//! It runs Session Runs through components that each own one responsibility
//! and serve the Session application roles their callers use: committed
//! Thread reads ([`ThreadReads`]), live inboxes ([`LiveInboxes`]), capability
//! assembly ([`RunCapabilities`]), child Runs ([`ChildRunLauncher`]), Session
//! context assembly ([`RunAssembly`]), Run delivery ([`SessionRuns`]),
//! coordination ([`ChildRunCoordination`]), Outcomes ([`Outcomes`]) and
//! budgets ([`BudgetAccounting`]). Every protocol drives the same components,
//! so none can create a protocol-specific run, resume, or terminal-state path.
//!
//! This crate consumes capabilities selected by process startup. It never opens
//! or selects a SQL/filesystem authority Store, never owns a public protocol or
//! domain aggregate, and never decides the deployment topology. Session resource
//! effects are supplied by `awaken-session-realization`. Coordinator
//! injects the sole durable [`awaken_dispatch_erasure::RuntimeAuthority`]; a database-less Worker is
//! constructed with an [`UpstreamAuthority`] carrying its claim-fenced
//! transports. Product composition mounts the corresponding routers around
//! these protocol-neutral behaviors.
//!
//! Declaration-owner source rules: A1/A2 the former public durable authority
//! names are absent here; the existing contract lives in dispatch-erasure.
//! These only test import capability, not Worker/store authorization. Original
//! startup, HostCommit and topology tables retain the runtime decisions.
//! ```compile_fail,E0432
//! use awaken_session_executor::RuntimeAuthority;
//! ```
//! ```compile_fail,E0432
//! use awaken_session_executor::RuntimeAuthorityError;
//! ```

mod acp_backend;
mod acp_capability_probe;
mod acp_tool_export;
mod agent_catalog;
mod agent_runner;
mod application;
pub use application::SessionRealizationRevocation;

mod authority;
mod auxiliary_sandbox;
mod background;
mod background_task;
mod commit_ingest;
mod compact;
mod config;
mod coordination;
mod delegate;
mod durable_operations;
mod session_execution_error;
pub use session_execution_error::{SessionExecutionError, SessionExecutionErrorKind, to_run_error};
mod session_effects;

#[cfg(test)]
mod environment_effects;
mod host;
mod hub;
mod inference_routing;
mod installed_session;
mod judge;
mod lazy_sandbox;
mod live_inbox;
pub use live_inbox::LiveInboxes;
mod managed_input_projection;
mod managed_model_capability;
mod managed_outcome;
mod mcp_host;
mod memory;
#[cfg(test)]
mod memory_stores;
mod model_content_materializer;
mod no_model;
mod outcome_controller;
mod outcome_report;
mod redact;
mod run_assembly;
pub use run_assembly::RunAssembly;
mod run_capabilities;
pub use run_capabilities::RunCapabilities;
mod child_run_launcher;
pub use child_run_launcher::ChildRunLauncher;
mod child_run_coordination;
pub use child_run_coordination::ChildRunCoordination;
mod outcomes;
pub use outcomes::Outcomes;
mod process_roles;
pub use process_roles::{CoordinatorRoles, WorkerRoles};
mod budget_accounting;
pub use budget_accounting::BudgetAccounting;
mod session_runs;
pub use session_runs::SessionRuns;
mod run_exec;
mod session_memory_tools;
mod session_tools;
mod skill_catalog;
mod skills;
mod step_projection;
mod store;
mod thread_reads;
pub use thread_reads::ThreadReads;

#[cfg(test)]
use awaken_session_mcp::test_mcp;
mod tool_output_spill;
mod unavailable_worker;
mod uninstalled_environment;
mod web_search;
mod worker_services;

pub(crate) use awaken_session_realization_contract::ResourceOperationFence;

#[cfg(any(test, feature = "test-support"))]
pub use crate::authority::EphemeralRuntimeAuthority;
pub use crate::authority::UpstreamAuthority;
pub(crate) use crate::authority::{LocalAuthority, SessionAuthority};
// Internal use of the canonical adapter owner; these are not public compatibility exports.
pub(crate) use crate::host::HostOutcomeIteration;
pub use crate::host::{CommittedStepReceipt, HostOutcomeDrive, HostOutcomeReport};
pub(crate) use crate::installed_session::InstalledSessions;
use awaken_dispatch_erasure::LocalCommit;
#[cfg(any(test, feature = "test-support"))]
use awaken_dispatch_erasure::LocalCommitAdapter;
// The neutral session substrate and its resume vocabulary.
pub use crate::acp_capability_probe::SessionAcpCapabilityNegotiator;
pub use crate::cache_volume::{
    CacheVolumeInitializer, CacheVolumeWarmup, SandboxCacheVolumeInitializer,
};
pub use crate::commit_ingest::session_commit_owner;
pub use crate::config::{advertised_tools, authorable_tools, block_text};
pub(crate) use crate::host::ApplicationPortBinding;
pub use crate::host::capability::WorkerContentAdapters;
#[cfg(any(test, feature = "test-support"))]
pub use crate::host::capability::{LocalResourcePorts, LocalSkillPorts, test_memory_content};
pub use crate::host::{
    AttemptExecutorDecorator, ExecutorComponents, HostResume, RemoteAttemptInstallation,
    SessionApplicationBindingError, SessionApplicationBindings, SessionExecutionDependencies,
    SessionExecutionProcess, SessionExecutorBuilder, TrustedHostPolicy, WarmPool,
};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
use awaken_credential_contract::CredentialRefreshFactory;
use awaken_credential_materializer::PinnedCredentialMaterializer;
pub(crate) use awaken_deployment_config::{
    DispatchPoolSizing, ExecutionTopology, LocalExecution, RunIngress,
};
use awaken_resource_contract::FileContentSource;
#[cfg(test)]
use awaken_resource_contract::RepositoryBindingVerifier;
pub(crate) use managed_input_projection::session_system_message;
#[cfg(test)]
pub(crate) use managed_input_projection::user_message;
/// Composition failure for the one process-local Session coordination port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentCoordinationInstallError {
    #[error("Session Agent coordination application is already installed")]
    AlreadyInstalled,
    #[error("Session dispatch observation source is unavailable")]
    DispatchObservationUnavailable,
    #[error("Session dispatch observation source differs from the canonical Runtime")]
    DispatchObservationMismatch,
    #[error("Session coordination application is unavailable at composition")]
    ApplicationUnavailable,
}

pub(crate) use awaken_session_configuration_contract::mcp as session_mcp;

pub(crate) use awaken_session_configuration_contract::resource as session_resource;

#[cfg(test)]
use awaken_session_realization::environment_owner;
use awaken_session_realization::{
    artifact_harvest, cache_volume, environment_holding, environment_providers,
    execution_authority, frozen_inputs, managed_input_layout, memory_binding, realization_error,
    realization_executor, realization_state, resource_staging, sandbox_projection,
    session_environment, session_publication, session_realizations, skill_delivery,
    staged_resources, terminal_effects,
};
#[cfg(test)]
#[path = "realization_composed_tests/mod.rs"]
mod realization_composed_tests;
