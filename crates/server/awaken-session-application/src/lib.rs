//! Coordinator-owned Session application state and ports.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use awaken_environment_contract::EnvItem;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
#[cfg(test)]
use awaken_session_contract::SessionExecutionState;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, McpTarget, PersistedSession, RunError,
    SandboxProvisioning, SessionEnvironmentBindingSink, SessionLifecycleFactSink, SessionRuntime,
    SessionRuntimePlacement,
};

mod mutation;
pub use mutation::SessionMutationError;
mod ports;
pub use ports::{
    RepositoryCredentialIngress, ResolvedSessionEnvironment, SessionCredentialSource,
    SessionEnvironmentSource,
};
include!("application.rs");
mod activity;
mod live_inbox;
pub use activity::SessionActivityError;
mod contribution;
mod creation;
pub use creation::{CreateSessionCommand, SessionCreationError};
mod credentials;
mod projection;
pub use credentials::SessionPreparationError;
mod mcp;
mod realization;
mod resource_reconciliation;
mod run_admission;
mod runtime_commands;
pub use mcp::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
pub use realization::{
    SessionRealizationError, SessionReconciliation, SessionReconciliationFailure,
};
pub use resource_reconciliation::SessionResourcePurgeGuard;
pub use run_admission::{
    AdmittedRunApplication, RecoveredSessionProjection, SessionProjectionRecoveryError,
    SessionRunAdmission,
};
pub use runtime_commands::SessionRepositoryResourceInput;
mod update;
pub use update::{
    SessionUpdateChanges, SessionUpdateCommand, SessionUpdateError, SessionUpdateOutcome,
};
mod terminal;
pub use terminal::SessionDispositionMutation;

#[cfg(test)]
mod tests;
