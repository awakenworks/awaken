// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Declares the Session application and its required consumer roles. Commands and
// @formal reason: authority checks are implemented by the owning application modules and contracts;
// @formal reason: these exports do not create another durable authority or prove external closure.
//! Coordinator-owned Session application state and required services.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(test)]
use awaken_environment_contract::EnvItem;
#[cfg(test)]
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_configuration_contract::EnvironmentSnapshot;
use awaken_session_contract::SessionEnvironmentFacts;
use awaken_session_contract::SessionRepository;
use awaken_session_execution_contract::SessionEnvironmentBindingSink;
use awaken_session_types::RunError;
use {
    awaken_session_contract::{PersistedSession, SessionScan},
    awaken_session_types::SessionRecoveryQuarantine,
};

pub use awaken_session_execution_contract::SessionExecutor;

mod execution_admission;
#[cfg(any(test, feature = "test-support"))]
pub use execution_admission::ExecutionObservationProbe;
pub use execution_admission::SessionExecutionAdmission;
mod agent_configuration;
pub use agent_configuration::{SessionRecoveryPurpose, agent_configuration_from_profile};
mod construction;
mod file_content_access;
pub use file_content_access::SessionFileContentAccess;
mod tool_compilation;
pub use construction::{SessionApplicationBuilder, SessionCredentialAuthorities, SessionResources};

mod recovery_scan;
pub use recovery_scan::{WorkerRealizationObservation, WorkerRealizationRead};

/// Single Session-application projection from repository policy into the Run
/// error vocabulary. Repository callers reuse the contract-owned recovery
/// disposition and never maintain variant lists beside this adapter.
fn session_repository_error(error: awaken_session_contract::SessionRepositoryError) -> RunError {
    use awaken_session_contract::{
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
    continuation_rejection: Option<awaken_session_contract::SessionEnvironmentRejection>,
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
            continuation_rejection: None,
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
    /// The continuation hint is the aggregate's `AdvanceContinuation`
    /// decision at the scan's `now`.
    fn from_session(session: &PersistedSession, audit_resource_references: bool, now: u64) -> Self {
        let continuation = awaken_session_contract::transition_environment(
            SessionEnvironmentFacts::project_continuation(session, now),
            awaken_session_contract::SessionEnvironmentEvent::AdvanceContinuation,
        );
        let environment_continuation =
            continuation.is_ok_and(|decision| decision.instruction().is_some());
        let continuation_rejection = continuation.err().filter(|rejection| {
            rejection.recovery_action()
                == awaken_session_contract::SessionEnvironmentReceiptRecoveryAction::Reject
        });
        let local_realization = !session.is_terminal()
            && session.environment.admits_realization_effects()
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
            continuation_rejection,
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

impl SessionRecoveryCandidates {
    fn append_page(&mut self, scan: SessionScan, audit_resource_references: bool, now: u64) {
        self.sessions
            .extend(scan.sessions.into_iter().map(|scoped| {
                let interests = SessionRecoveryInterests::from_session(
                    &scoped.session,
                    audit_resource_references,
                    now,
                );
                SessionRecoveryCandidate {
                    workspace_id: scoped.workspace_id,
                    session_id: scoped.session.session_id,
                    interests,
                }
            }));
    }
}

mod mutation;
mod participant_provenance;
mod recovery_sweep;
mod sandbox_observation;
pub use mutation::{SessionMutationError, SessionMutations, SessionStateViolation, SnapshotCommit};
pub use participant_provenance::SessionParticipantProvenance;
mod event_batch_cutover_validation;
pub use event_batch_cutover_validation::{
    SessionEventBatchCutoverValidationSnapshot, SessionEventBatchCutoverValidationSource,
};
include!("application.rs");
mod access;
pub use access::{SessionAccess, SessionAccessError};
mod activity;
mod inputs;
mod lifecycle_outbox_relay;
pub use activity::{SessionActivities, SessionActivityError};
pub use inputs::{PreparedSessionInputs, SessionInputs};
pub use lifecycle_outbox_relay::{
    LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL, LifecycleOutboxRelay, LifecycleOutboxReplay,
};
mod continuation;
mod coordination;
pub use coordination::SessionCoordination;
mod creation;
mod session_input;
pub use continuation::{SessionContinuationError, SessionContinuations};
pub use creation::{
    CreateSessionCommand, SessionCreateReceipt, SessionCreationCommands, SessionCreationError,
};
pub use creation::{
    SessionBudgetRequest, SessionCreateProtocol, SessionCreateRequest, SessionCreateRequestError,
    SessionModelRequest, SessionToolsOverride,
};
pub use session_input::{SessionInputRequest, SessionInputTarget};
mod credentials;
mod event_batches;
pub use event_batches::{
    SessionEventBatchCommands, SessionEventBatchIdempotency, SessionEventBatchResult,
};
mod outcome_reconciliation;
pub use outcome_reconciliation::SessionOutcomeReconciliation;
mod projection;
mod queries;
pub use awaken_session_execution_contract::{
    CommittedSessionFacts, ProjectionThreadSnapshots, SessionProjectionReadTimings,
};
pub use credentials::{SessionCredentials, SessionPreparationError};
mod lifecycle;
pub use lifecycle::{BudgetSettlementOutcome, SessionDeleteCommand, SessionLifecycle};
mod recovery;
pub use queries::SessionQueries;
pub use recovery::SessionRecovery;
mod worker_occupancy;
pub use worker_occupancy::{
    SessionWorkerOccupancy, WorkerRetirementBlockReason, WorkerRetirementBlocker,
};
mod mcp;
mod realization;
mod resource_lowering;
mod resource_reconciliation;
mod roles;
mod run_admission;
mod runtime_commands;
pub use awaken_session_realization_driver::{
    SessionTerminalCleanupDriveOutcome, drive_session_realization, drive_session_terminal_cleanup,
};
pub use mcp::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
pub use realization::{SessionRealizations, SessionReconciliation, SessionReconciliationFailure};
mod resource_effect_authorization;
pub use resource_effect_authorization::SessionResourceEffectAuthorization;
mod resource_references;
pub use resource_references::SessionResourceReferences;
mod terminal_cleanup;
pub use terminal_cleanup::SessionTerminalCleanups;
mod observations;
pub use observations::SessionObservations;
mod work_assignments;
pub(crate) use resource_effect_authorization::require_live_root_lease;
mod resource_manifests;
pub use creation::{CreateProfiledSessionCommand, ProfiledSessionRepositoryInput};
pub use resource_manifests::{
    ReplaceSessionResourceManifest, SessionResourceAttachment, SessionResourceManifestError,
    SessionResourceManifestOutcome, SessionResourceManifestRequest, SessionResourceManifests,
};
pub use resource_reconciliation::{SessionResourcePurgeGuard, SessionResourceReconciliation};
pub use roles::{
    DeploymentSessionEnvironment, ProfiledSessionCommands, ProfiledSessionReplies, SessionCreation,
    SessionCredentialCommands, SessionEventBatches, SessionObservation, SessionProjectionReads,
    SessionResourceCommands, SessionRootReads, SessionTermination, SessionUpdates,
};
pub use run_admission::{
    RecoveredSessionProjection, SessionProjectionRecoveryError, SessionRunAdmissions,
    SessionRunApplication,
};
pub use runtime_commands::{
    SessionExecutionObservation, SessionModelRequestObservation, SessionRunExecutionObservation,
    SessionToolUseObservation,
};
mod repository_participants;
pub use repository_participants::{
    ConfiguredSessionRepository, SessionRepositoryOwner, SessionRepositoryParticipants,
    SessionRepositoryResourceInput,
};
pub use work_assignments::SessionWorkAssignments;
mod update;
pub use update::{
    SessionMcpUpdate, SessionMetadataUpdate, SessionUpdateCommand, SessionUpdateCommands,
    SessionUpdateError, SessionUpdateOutcome,
};
mod archive_intent;
mod terminal;
pub use terminal::{
    SessionArchiveWithRepositoryPublicationCommand, SessionArchiveWithRepositoryPublicationError,
    SessionArchiveWithRepositoryPublicationOutcome, SessionDispositionMutation,
    SessionRepositoryPublicationSelector, SessionTerminationCommands,
};

#[cfg(test)]
mod tests;

use awaken_session_contract::SessionAggregate;

mod configuration;
pub use configuration::{
    SessionApplicationBuildError, SessionApplicationConfiguration, SessionExecutionPlacement,
    WorkDispatchFailure, WorkDispatchReconciliation, runtime_placement_for_topology,
};

pub type ResolvedSessionEnvironment =
    awaken_executable_environment_contract::ResolvedSessionEnvironment<EnvironmentSnapshot>;
use awaken_executable_environment_contract::SessionEnvironmentProjectionSource;

/// Required Environment roles projected from one coherent application source.
/// Construction is explicit and has no optional capability discovery.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_executable_environment_contract::SessionEnvironmentProjectionSource;
/// use awaken_session_application::SessionEnvironments;
/// fn query_only<T: SessionEnvironmentProjectionSource<Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot> + 'static>(source: Arc<T>) {
///     let _: SessionEnvironments = source.into();
/// }
/// ```
#[derive(Clone)]
pub struct SessionEnvironments {
    projection: Arc<dyn SessionEnvironmentProjectionSource<Snapshot = EnvironmentSnapshot>>,
    assignment: Arc<dyn awaken_session_application_contract::SessionWorkerAssignment>,
    leases: Arc<dyn awaken_session_application_contract::SessionWorkerLeases>,
}
impl<T> From<Arc<T>> for SessionEnvironments
where
    T: SessionEnvironmentProjectionSource<Snapshot = EnvironmentSnapshot>
        + awaken_session_application_contract::SessionWorkerAssignment
        + awaken_session_application_contract::SessionWorkerLeases
        + 'static,
{
    fn from(source: Arc<T>) -> Self {
        Self {
            projection: source.clone(),
            assignment: source.clone(),
            leases: source,
        }
    }
}

pub(crate) use awaken_session_configuration_contract::mcp as session_mcp;

pub(crate) use awaken_session_configuration_contract::resource as session_resource;

#[cfg(test)]
use awaken_session_contract::{SessionFieldUpdate, SessionUpdateChanges};
