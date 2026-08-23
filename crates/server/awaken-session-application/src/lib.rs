//! Coordinator-owned Session application state and required services.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use awaken_environment_contract::EnvItem;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
#[cfg(test)]
use awaken_session_contract::SessionExecutionState;
use awaken_session_contract::{
    LifecycleFactNotifier, ManagedSessionRepository, McpAttachmentRealizer, McpTarget,
    PersistedSession, RunError, SandboxProvisioning, SessionEnvironmentBindingSink, SessionRuntime,
    SessionRuntimePlacement,
};

mod mutation;
pub use mutation::SessionMutationError;
mod session_services;
pub use session_services::{
    RepositoryCredentialIngress, ResolvedSessionEnvironment, SessionCredentialSource,
    SessionEnvironmentSource,
};
include!("application.rs");
mod activity;
mod budget;
mod live_inbox;
pub use activity::{SessionActivityError, SessionMessageOutcome};
pub use budget::BudgetSettlementOutcome;
mod continuation;
mod coordination;
mod creation;
pub use continuation::SessionContinuationError;
pub use creation::{CreateSessionCommand, SessionCreationError};
mod credentials;
mod event_batches;
mod outcome_reconciliation;
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
pub use resource_reconciliation::{
    ReplaceSessionResourceManifest, SessionResourceManifestError, SessionResourceManifestOutcome,
    SessionResourcePurgeGuard,
};
pub use run_admission::{
    AdmittedRunApplication, CreateProfiledSessionCommand, RecoveredSessionProjection,
    SessionProjectionRecoveryError, SessionRunAdmission,
};
pub use runtime_commands::SessionRepositoryResourceInput;
mod update;
pub use update::{
    SessionFieldUpdate, SessionMetadataUpdate, SessionUpdateChanges, SessionUpdateCommand,
    SessionUpdateError, SessionUpdateOutcome,
};
mod terminal;
pub use terminal::{SessionDeleteCommand, SessionDispositionMutation};

#[cfg(test)]
mod tests;
