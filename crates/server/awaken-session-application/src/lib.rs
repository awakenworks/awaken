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

/// Drain the repository's one typed keyset index before any reconciler starts
/// effects. The cursor is process-local loop state only: the repository remains
/// the sole durable scheduling and row authority.
async fn scan_all_reconcilable_sessions(
    repository: &dyn ManagedSessionRepository,
) -> Result<SessionRecoveryScan, awaken_session_contract::SessionRepositoryError> {
    let mut complete = SessionRecoveryScan::default();
    let mut quarantined = std::collections::BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = repository
            .reconcilable_sessions_page(cursor.as_ref())
            .await?;
        let next_cursor = page.next_cursor;
        if let (Some(current), Some(next)) = (cursor.as_ref(), next_cursor.as_ref())
            && next.session_id() <= current.session_id()
        {
            return Err(awaken_session_contract::SessionRepositoryError::Corrupt(
                "Session reconciliation cursor did not advance".into(),
            ));
        }
        complete.sessions.extend(page.sessions);
        for isolation in page.quarantined {
            quarantined.insert(isolation.session_id.clone(), isolation);
        }
        let Some(next_cursor) = next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    complete.quarantined = quarantined.into_values().collect();
    Ok(complete)
}

/// Single Session-application projection from repository policy into the Run
/// error vocabulary. Repository callers reuse the contract-owned recovery
/// disposition and never maintain variant lists beside this adapter.
fn session_repository_error(error: awaken_session_contract::SessionRepositoryError) -> RunError {
    use awaken_session_contract::{
        SessionRepositoryError as Error, SessionRepositoryRecoveryAction as Action,
    };

    match (&error, error.recovery_action()) {
        (Error::NotFound, _) => RunError::bad_request("Session was not found"),
        (Error::Unavailable(message), Action::Retry) => RunError::unavailable(message.clone()),
        (_, Action::Retry) => {
            RunError::unavailable_classified("session_repository_unavailable", error.to_string())
        }
        (_, Action::Quarantine) => {
            RunError::classified("session_repository_corrupt", error.to_string())
        }
        (_, Action::Reject) => {
            RunError::classified("session_repository_rejected", error.to_string())
        }
    }
}

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
    CredentialMaterialIngress, CredentialMaterialIngressCommand, CredentialMaterialIngressReceipt,
    CredentialMaterialInput, CredentialMaterialRetirementCommand,
    CredentialMaterialRotationCommand, CredentialPlaintext, ResolvedSessionEnvironment,
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
pub use activity::SessionActivityError;
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
    CreateProfiledSessionCommand, ProfiledSessionRepositoryInput, RecoveredSessionProjection,
    SessionProjectionRecoveryError, SessionRunApplication,
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
pub use terminal::{
    SessionArchiveWithRepositoryPublicationCommand, SessionArchiveWithRepositoryPublicationError,
    SessionArchiveWithRepositoryPublicationOutcome, SessionDeleteCommand,
    SessionDispositionMutation, SessionRepositoryPublicationSelector,
};

#[cfg(test)]
mod tests;
