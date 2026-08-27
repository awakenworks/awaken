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
    PersistedSession, RunError, SandboxProvisioning, SessionEnvironmentBindingSink,
    SessionRecoveryQuarantine, SessionRecoveryScan, SessionRuntime, SessionRuntimePlacement,
};

/// Identity-only input to one recovery cycle. Reconcilers must reload the root
/// before deciding or performing an effect; carrying a `PersistedSession` here
/// would make a stale pre-reconciliation snapshot too easy to reuse.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionRecoveryCandidate {
    workspace_id: String,
    session_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionRecoveryCandidates {
    sessions: Vec<SessionRecoveryCandidate>,
    quarantined: Vec<SessionRecoveryQuarantine>,
}

impl From<SessionRecoveryScan> for SessionRecoveryCandidates {
    fn from(scan: SessionRecoveryScan) -> Self {
        Self {
            sessions: scan
                .sessions
                .into_iter()
                .map(|scoped| SessionRecoveryCandidate {
                    workspace_id: scoped.workspace_id,
                    session_id: scoped.session.session_id,
                })
                .collect(),
            quarantined: scan.quarantined,
        }
    }
}

mod mutation;
pub use mutation::SessionMutationError;
mod session_services;
pub use session_services::{
    RepositoryCredentialEntry, RepositoryCredentialIngress, ResolvedSessionEnvironment,
    SessionCredentialAccessRequest, SessionCredentialSource, SessionEnvironmentSource,
    SessionParticipantProvenance,
};
mod event_batch_cutover_validation;
pub use event_batch_cutover_validation::{
    SessionEventBatchCutoverValidationSnapshot, SessionEventBatchCutoverValidationSource,
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
pub use event_batches::SessionEventBatchIdempotency;
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
pub use runtime_commands::{
    ConfiguredSessionRepository, SessionRepositoryOwner, SessionRepositoryResourceInput,
};
mod update;
pub use update::{
    SessionFieldUpdate, SessionMcpUpdate, SessionMetadataUpdate, SessionUpdateChanges,
    SessionUpdateCommand, SessionUpdateError, SessionUpdateOutcome,
};
mod terminal;
pub use terminal::{SessionDeleteCommand, SessionDispositionMutation};

#[cfg(test)]
mod tests;
