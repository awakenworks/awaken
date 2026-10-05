//! Coordinator-owned Session application state and required services.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use awaken_environment_contract::EnvItem;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_configuration_contract::{SandboxProvisioning, SessionRuntimePlacement};
use awaken_session_error_contract::RunError;
use awaken_session_mcp_contract::McpTarget;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_execution_contract::{
    CompleteSessionRuntime, McpAttachmentRealizer, SessionEnvironmentBindingSink,
};
#[cfg(test)]
use awaken_session_state_contract::SessionExecutionState;
use awaken_session_state_contract::{PersistedSession, SessionRecoveryQuarantine, SessionScan};

/// Drain the repository's one typed keyset index before any reconciler starts
/// effects. The cursor is process-local loop state only: the repository remains
/// the sole durable scheduling and row authority.
async fn scan_all_reconcilable_sessions(
    repository: &dyn SessionRepository,
) -> Result<SessionScan, awaken_session_state_contract::SessionRepositoryError> {
    let mut complete = SessionScan::default();
    let mut quarantined = std::collections::BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = repository
            .scan(
                awaken_session_state_contract::SessionScanQuery::Reconcilable {
                    after: cursor.as_ref(),
                },
            )
            .await?;
        let next_cursor = page.next_cursor;
        if let (Some(current), Some(next)) = (cursor.as_ref(), next_cursor.as_ref())
            && next.session_id() <= current.session_id()
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::Corrupt(
                    "Session reconciliation cursor did not advance".into(),
                ),
            );
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
fn session_repository_error(
    error: awaken_session_state_contract::SessionRepositoryError,
) -> RunError {
    use awaken_session_state_contract::{
        SessionRepositoryError as Error, SessionRepositoryRecoveryAction as Action,
    };

    match (&error, error.recovery_action()) {
        (Error::NotFound, _) => RunError::bad_request("Session was not found"),
        (Error::AuthorityUnavailable(message), Action::Retry) => {
            RunError::unavailable(message.clone())
        }
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
    session_id: awaken_session_types::SessionId,
    interests: SessionRecoveryInterests,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionRecoveryInterests {
    resources: bool,
    resource_activation: bool,
    event_batches: bool,
    environment_continuation: bool,
    local_realization: bool,
    dispatch_projection: bool,
}

impl Default for SessionRecoveryInterests {
    fn default() -> Self {
        Self {
            resources: true,
            resource_activation: true,
            event_batches: true,
            environment_continuation: true,
            local_realization: true,
            dispatch_projection: true,
        }
    }
}

impl SessionRecoveryInterests {
    /// Read-only scheduling hints from the repository's atomic scan prefix.
    /// A true hint still requires the phase's ordinary fresh root read before
    /// any effect. A concurrent transition after this prefix retains its wake
    /// permit and is reconsidered by the next cycle.
    fn from_session(session: &PersistedSession, audit_resource_references: bool) -> Self {
        let environment_continuation = !session.is_terminal()
            && (matches!(
                session.environment,
                awaken_session_state_contract::SessionEnvironmentState::Suspending { .. }
            ) || (session.execution
                == awaken_session_state_contract::SessionExecutionState::Idle
                && matches!(
                    session.environment,
                    awaken_session_state_contract::SessionEnvironmentState::Resident {
                        idle_since_unix_ms: Some(_),
                        ..
                    }
                )));
        let local_realization = !session.is_terminal()
            && realization::environment_admits_realization_effects(&session.environment)
            && !session.requires_external_environment_owner();
        let dispatch_projection = !session.is_terminal()
            && session.needs_initial_realization_recovery()
            && session.requires_external_environment_owner();
        Self {
            resources: session.needs_resource_reconciliation()
                || (audit_resource_references && session.resources.has_references()),
            resource_activation: !session.is_terminal() && session.resources.needs_reconciliation(),
            event_batches: session.needs_event_reconciliation(),
            environment_continuation,
            local_realization,
            dispatch_projection,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionRecoveryCandidates {
    sessions: Vec<SessionRecoveryCandidate>,
    quarantined: Vec<SessionRecoveryQuarantine>,
}

impl From<SessionScan> for SessionRecoveryCandidates {
    fn from(scan: SessionScan) -> Self {
        Self::from_scan(scan, true)
    }
}

impl SessionRecoveryCandidates {
    fn from_scan(scan: SessionScan, audit_resource_references: bool) -> Self {
        Self {
            sessions: scan
                .sessions
                .into_iter()
                .map(|scoped| {
                    let interests = SessionRecoveryInterests::from_session(
                        &scoped.session,
                        audit_resource_references,
                    );
                    SessionRecoveryCandidate {
                        workspace_id: scoped.workspace_id,
                        session_id: scoped.session.session_id,
                        interests,
                    }
                })
                .collect(),
            quarantined: scan.quarantined,
        }
    }
}

mod mutation;
mod recovery_sweep;
pub use mutation::{SessionMutationError, SnapshotCommit};
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
mod lifecycle_outbox_relay;
mod live_inbox;
pub use activity::SessionActivityError;
pub use budget::BudgetSettlementOutcome;
pub use lifecycle_outbox_relay::{
    LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL, LifecycleOutboxRelay, LifecycleOutboxReplay,
};
mod continuation;
mod coordination;
mod creation;
mod creation_request;
mod session_input;
pub use continuation::SessionContinuationError;
pub use creation::{CreateSessionCommand, SessionCreationError};
pub use creation_request::{
    CreatedSession, SessionBudgetRequest, SessionCreateProtocol, SessionCreateRequest,
    SessionCreateRequestError, SessionModelChoice, SessionModelRequest, SessionModelSource,
    SessionToolsOverride,
};
pub use session_input::{
    InputProjectionClass, MAX_MEMORY_INSTRUCTIONS_CHARS, MAX_SESSION_FILE_RESOURCES,
    MAX_SESSION_MEMORY_STORES, PreparedSessionInputs, RepositoryAccessToken, SessionInputRequest,
    SessionInputTarget, admit_file_count, admit_memory_inputs, input_binding,
    memory_mount_component, preflight_repository_mount_paths, unique_memory_mount_path,
};
mod credentials;
mod event_batches;
pub use event_batches::SessionEventBatchIdempotency;
mod outcome_reconciliation;
mod projection;
mod projection_evidence;
pub use awaken_managed_session_events::{
    CommittedSessionFacts, ProjectionThreadSnapshots, SessionProjectionReadTimings,
};
pub use credentials::SessionPreparationError;
mod mcp;
mod realization;
mod resource_reconciliation;
mod roles;
mod run_admission;
mod run_events;
mod runtime_commands;
pub use awaken_session_realization_driver::{
    SessionTerminalCleanupDriveOutcome, drive_session_realization, drive_session_terminal_cleanup,
};
pub use mcp::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
pub use realization::{
    SessionRealizationError, SessionReconciliation, SessionReconciliationFailure,
};
pub use resource_reconciliation::{
    ReplaceSessionResourceManifest, SessionResourceManifestError, SessionResourceManifestOutcome,
    SessionResourcePurgeGuard,
};
pub use roles::{
    DeploymentSessionEnvironment, ProfiledSessionCommands, SessionCreation, SessionEventBatches,
    SessionObservation, SessionProjectionReads, SessionResourceCommands, SessionRootReads,
    SessionTermination, SessionUpdates,
};
pub use run_admission::{
    CreateProfiledSessionCommand, ProfiledSessionRepositoryInput, RecoveredSessionProjection,
    SessionProjectionRecoveryError, SessionRunApplication,
};
pub use runtime_commands::{
    ConfiguredSessionRepository, SessionExecutionObservation, SessionModelRequestObservation,
    SessionRepositoryOwner, SessionRepositoryResourceInput, SessionRunExecutionObservation,
    SessionToolUseObservation,
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
