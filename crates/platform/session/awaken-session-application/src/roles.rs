// @formal surface exclude=application_adapter
// @formal owner crates/platform/session/awaken-session-application/src/mutation.rs
// @formal owner crates/platform/session/awaken-session-application/src/terminal.rs
// @formal owner crates/platform/session/awaken-session-application/src/resource_reconciliation.rs
// @formal owner crates/platform/session/awaken-session-application/src/credentials.rs
// @formal owner crates/platform/session/awaken-session-application/src/application.rs
// @formal owner crates/platform/session/awaken-session-application/src/runtime_commands.rs
// @formal owner crates/platform/session/awaken-session-configuration-contract/src/resource/inputs.rs
// @formal reason: Consumer roles adapt the selected commands and original resolver/layout kernels;
// @formal reason: Resource resolution and layout validation have no duplicate inherent entrances.
// @formal reason: Root CAS, cleanup, intrinsic errors and credential decisions retain their owners.
//! The Session application's roles, each cut to the adapter code that calls it.
//!
//! A protocol adapter holds the roles it calls instead of the whole
//! application, so what an adapter may do is visible in its fields and a role
//! can be supplied by another implementation without the rest. Each trait names
//! its consumers; a method belongs to exactly one role. The commands, outcomes
//! and errors these roles speak are owned by this crate, so the traits live
//! beside them rather than in a contract crate.
//!
//! Each role is bound to its owning application service. Resource resolution
//! and layout validation directly adapt their existing kernels. Other commands
//! use their owning command. Termination-only role lowering selects the existing
//! archive kernel without extending the broad inherent application surface.

use awaken_credential_contract::CredentialMaterialInput;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_environment_contract::EnvItem;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::ExecutableProjectionAdmission;
use awaken_session_contract::SessionEventAdmission;
use awaken_session_contract::SessionEventInput;
use awaken_session_contract::SessionUsage;
use awaken_session_contract::{
    DeletedSessionEventAdmissions, IdempotencyRecord, PersistedSession, SessionRepositoryError,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, CoordinatedThreadLink, SessionRunActivation,
    SessionThreadLiveSubscription,
};
use awaken_session_types::SessionId;

use crate::{
    CommittedSessionFacts, CreateProfiledSessionCommand, ProjectionThreadSnapshots,
    RecoveredSessionProjection, SessionArchiveWithRepositoryPublicationCommand,
    SessionArchiveWithRepositoryPublicationError, SessionArchiveWithRepositoryPublicationOutcome,
    SessionCreateProtocol, SessionCreateRequest, SessionCreateRequestError, SessionCreationError,
    SessionDeleteCommand, SessionDispositionMutation, SessionEventBatchIdempotency,
    SessionExecutionObservation, SessionMutationError, SessionPreparationError,
    SessionProjectionRecoveryError, SessionResourceAttachment, SessionResourceManifestError,
    SessionResourceManifestOutcome, SessionResourceManifestRequest, SessionUpdateCommand,
    SessionUpdateError, SessionUpdateOutcome,
};

/// Create a Session from a protocol-lowered request, and replay a create.
///
/// The original complete Session root is the sole successful result.
///
/// Consumer: a protocol's Session create command and its create-retry replay.
#[async_trait]
pub trait SessionCreation: Send + Sync {
    /// Advance executable facts before entering a lifecycle command. This is
    /// admission with effects, not a Session query.
    async fn refresh_executable_projections(&self)
    -> Result<ExecutableProjectionAdmission, String>;

    /// See [`SessionApplication::create_requested_session`].
    async fn create_requested_session(
        &self,
        request: SessionCreateRequest,
        protocol: &dyn SessionCreateProtocol,
    ) -> Result<PersistedSession, SessionCreateRequestError>;

    /// Current trusted private ingress supplies the binding; wire fields and
    /// Workspace grants cannot supply private authority. Unsupported owners
    /// must stay unavailable instead of using ordinary creation.
    async fn create_requested_private_session(
        &self,
        _binding: awaken_session_contract::SessionPrivateBinding,
        _request: SessionCreateRequest,
        _protocol: &dyn SessionCreateProtocol,
    ) -> Result<PersistedSession, SessionCreateRequestError> {
        Err(awaken_runtime_contract::runs::RunError::unavailable(
            "private Session creation is unavailable",
        )
        .into())
    }

    /// See [`crate::SessionMutations::replay_session_create`].
    async fn replay_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError>;

    /// See [`crate::SessionMutations::replay_accepted_session_create`].
    async fn replay_accepted_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError>;

    /// Recover the original private root only after current private admission.
    async fn replay_private_session_create(
        &self,
        _binding: &awaken_session_contract::SessionPrivateBinding,
        _session_id: &SessionId,
        _idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        Err(SessionCreationError::Unavailable(
            "private Session create replay is unavailable".into(),
        ))
    }

    /// The private counterpart of asynchronous create recovery.
    async fn replay_accepted_private_session_create(
        &self,
        _binding: &awaken_session_contract::SessionPrivateBinding,
        _session_id: &SessionId,
        _idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        Err(SessionCreationError::Unavailable(
            "private Session create replay is unavailable".into(),
        ))
    }

    /// The process-unique Runtime incarnation a protocol folds into its fresh
    /// Session id candidates.
    fn runtime_incarnation(&self) -> &str;
}

/// Read durable Session roots.
///
/// Consumer: a protocol's projection refresh, event admission, Resource
/// commands, Session listing, Dream eligibility and credential rollout.
#[async_trait]
pub trait SessionRootReads: Send + Sync {
    /// The durable root of one Session.
    async fn session(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError>;

    /// The Workspace that owns one Session.
    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionMutationError>;

    /// Workspace-visible linked content requires the native root/tombstone
    /// verdict. This supplies no private or Worker execution authority.
    async fn workspace_content_is_public(
        &self,
        _owner_scope: &str,
        _session_id: &SessionId,
    ) -> Result<bool, SessionRepositoryError> {
        Err(SessionRepositoryError::AuthorityUnavailable(
            "retained Session content visibility is unavailable".into(),
        ))
    }

    /// The one ownership verdict owner-scoped commands accept: a foreign and
    /// a missing Session are the same `NotFound`. Private roots require the
    /// separate exact private binding, regardless of Workspace privileges.
    async fn authorize(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<crate::SessionAccess, crate::SessionAccessError>;

    /// The caller must obtain this binding through current trusted ingress;
    /// decoding a binding does not authenticate its owner. Implementations
    /// without the native private ownership mechanism fail closed.
    async fn authorize_private(
        &self,
        _binding: &awaken_session_contract::SessionPrivateBinding,
        _session_id: &SessionId,
    ) -> Result<crate::SessionAccess, crate::SessionAccessError> {
        Err(crate::SessionAccessError::NotFound)
    }

    /// Every visible root one Workspace owns.
    async fn sessions_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError>;

    /// Exact private collection after current trusted ingress admission.
    /// Unsupported implementations must never use the Workspace collection.
    async fn private_sessions(
        &self,
        _binding: &awaken_session_contract::SessionPrivateBinding,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        Err(SessionRepositoryError::AuthorityUnavailable(
            "private Session collection is unavailable".into(),
        ))
    }

    /// Healthy roots in one Workspace whose MCP attachments reference one
    /// credential source.
    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError>;

    /// The deletion tombstone of one Session inside its owner partition.
    /// `None` means no matching tombstone, not proof the Session exists.
    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError>;
}

/// Read committed evidence for Session projection and exact-Run observations.
///
/// Consumer: a protocol's committed projection refresh, cold rehydration and
/// Session projection reads.
#[async_trait]
pub trait SessionProjectionReads: Send + Sync {
    /// Recover the original recorded or legacy configuration without effects.
    /// The returned baseline borrows this same root; missing exact publications
    /// retain the original interactive/frozen recovery law.
    fn recover_agent_configuration<'a>(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: &'a PersistedSession,
        purpose: crate::SessionRecoveryPurpose,
    ) -> Result<
        (
            &'a awaken_session_configuration_contract::SessionBaseline,
            awaken_session_configuration_contract::SessionAgentConfiguration,
        ),
        RunError,
    >;

    /// See [`crate::SessionQueries::read_projection_evidence`].
    async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<CommittedSessionFacts, RunError>;

    /// See [`crate::SessionQueries::projection_thread_snapshot`].
    async fn projection_thread_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
        selected_run: Option<&awaken_agent_contract::RunId>,
    ) -> Result<Option<RunRecoverySnapshot>, RunError>;

    /// See [`crate::SessionQueries::projection_coordinated_prefix`].
    async fn projection_coordinated_prefix(
        &self,
        session_id: &SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError>;

    /// Revalidate the existing complete prefix and root-revision fence.
    async fn projection_prefix_moved(
        &self,
        persisted: &PersistedSession,
        prefix: awaken_session_execution_contract::SessionProjectionPrefix<'_>,
    ) -> Result<bool, RunError>;

    /// See [`crate::SessionQueries::read_session_projection`].
    async fn read_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError>;
}

/// Admit Event batches into a Session.
///
/// Consumer: a protocol's Event append command and its admission-close retry.
/// An append returns `None` for an accepted no-op that left no root fact: an
/// interrupt-only batch on a budget-paused Session with nothing Awaiting.
#[async_trait]
pub trait SessionEventBatches: Send + Sync {
    /// See [`SessionApplication::append_and_drive_session_event_batch_idempotent`].
    async fn append_and_drive_session_event_batch_idempotent(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(crate::SessionEventBatchResult, Option<RunError>), RunError>;

    /// See [`SessionApplication::admit_session_event_batch_idempotent`].
    async fn append_session_event_batch_idempotent(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<crate::SessionEventBatchResult, RunError>;

    /// See [`SessionApplication::session_event_batch_idempotency`].
    async fn session_event_batch_idempotency(
        &self,
        session_id: &SessionId,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError>;

    /// See [`SessionApplication::close_session_event_admission`].
    async fn close_session_event_admission(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        key: String,
        request_fingerprint: String,
    ) -> Result<SessionEventAdmission, RunError>;

    /// Whether the Control projection marks an Agent unavailable.
    fn agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool;

    /// Whether the Runtime on one Thread accepts a system message mid-conversation.
    async fn supports_mid_conversation_system(&self, thread: &str) -> Result<bool, RunError>;
}

/// Create and run product-profiled Sessions.
///
/// Consumer: the profiled Session create and Run edges.
#[async_trait]
pub trait ProfiledSessionCommands: Send + Sync {
    /// Request the existing root-owned preparation without creating a Run.
    async fn request_run_free_initial_preparation(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, awaken_session_execution_contract::SessionRealizationControlFailure>;

    /// See [`crate::SessionCreationCommands::accept_profiled_session`].
    async fn accept_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError>;

    /// See [`crate::SessionRunAdmissions::admit_profiled_session_run`].
    async fn admit_profiled_session_run(
        &self,
        access: &crate::SessionAccess,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, SessionProjectionRecoveryError>;

    /// See [`crate::SessionRunAdmissions::activate_admitted_session_run`].
    async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<SessionRunActivation, RunError>;

    /// See [`crate::SessionRunAdmissions::session_run_events_after`].
    async fn session_run_events_after(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
        dispatch_after: awaken_dispatch_contract::DispatchCursor,
        lifecycle_after: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_dispatch_contract::SessionRunEventPage, SessionProjectionRecoveryError>;
}

/// Reply to a pending tool request of a profiled Session's primary Run.
///
/// Consumer: the profiled Session Run reply edge.
#[async_trait]
pub trait ProfiledSessionReplies: Send + Sync {
    /// See [`SessionApplication::reply_profiled_session_run_for_owner`].
    async fn reply_profiled_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), SessionProjectionRecoveryError>;
}

/// End a Session or one of its Threads.
///
/// Consumer: a protocol's archive, delete, archive-with-publication and Thread
/// archive commands.
#[async_trait]
pub trait SessionTermination: Send + Sync {
    /// See [`SessionApplication::archive_session`].
    async fn archive_session(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError>;

    /// See [`SessionApplication::release_profiled_session_with_verified_cleanup`].
    async fn release_profiled_session(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError>;

    /// See [`SessionApplication::delete_session`]. The detached cleanup it
    /// starts keeps the application alive, so the role is taken by `Arc`.
    async fn delete_session(
        self: Arc<Self>,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError>;

    /// See [`SessionApplication::archive_with_repository_publication`].
    async fn archive_with_repository_publication(
        &self,
        command: SessionArchiveWithRepositoryPublicationCommand,
    ) -> Result<
        SessionArchiveWithRepositoryPublicationOutcome,
        SessionArchiveWithRepositoryPublicationError,
    >;

    /// Archive one idle Thread of a Session.
    async fn archive_session_thread(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<(), RunError>;
}

/// Change a Session's mutable fields.
///
/// Consumer: a protocol's Session update command and credential rollout
/// adoption.
#[async_trait]
pub trait SessionUpdates: Send + Sync {
    /// See [`SessionApplication::update_session`].
    async fn update_session(
        &self,
        session_id: &SessionId,
        command: SessionUpdateCommand,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError>;
}

/// Replace, attach and detach a Session's Resources.
///
/// The protocol lowers its wire resources into input requests and projects the
/// returned outcome; identity, reuse, admission, layout, Repository
/// participants and compensation are decided by the application.
///
/// Consumer: a protocol's Session Resource commands.
#[async_trait]
pub trait SessionResourceCommands: Send + Sync {
    /// See [`SessionApplication::replace_session_resource_manifest_request`].
    async fn replace_session_resource_manifest(
        &self,
        session_id: &SessionId,
        request: SessionResourceManifestRequest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError>;

    /// See [`SessionApplication::attach_session_file`].
    async fn attach_session_file(
        &self,
        session_id: &SessionId,
        request: crate::SessionInputRequest,
    ) -> Result<SessionResourceAttachment, SessionResourceManifestError>;

    /// See [`SessionApplication::detach_session_input`].
    async fn detach_session_input(
        &self,
        session_id: &SessionId,
        binding_id: &awaken_resource_contract::BindingId,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError>;
}

/// Observe a Session's live and committed execution.
///
/// Consumer: a protocol's live event stream, execution evidence read and Dream
/// usage read.
#[async_trait]
pub trait SessionObservation: Send + Sync {
    async fn transcript_page(
        &self,
        access: &crate::SessionAccess,
        request: &awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
    ) -> Result<awaken_agent_contract::thread::read::transcript::TranscriptPage, RunError>;

    /// Adapt the selected [`awaken_session_execution_contract::ChildRunCoordination::subscribe_session_thread_live`] result unchanged.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError>;

    /// See [`SessionApplication::session_execution_observation`].
    async fn session_execution_observation(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
    ) -> Result<SessionExecutionObservation, SessionProjectionRecoveryError>;

    /// See [`SessionApplication::session_usage`].
    async fn session_usage(
        &self,
        thread: &str,
        observed_links: Option<&[CoordinatedThreadLink]>,
    ) -> Result<SessionUsage, RunError>;
}

/// Read what a Deployment launch needs from the Session application.
///
/// Consumer: a protocol's Deployment admission.
#[async_trait]
pub trait DeploymentSessionEnvironment: Send + Sync {
    /// Read the selected [`awaken_executable_environment_contract::SessionEnvironmentProjectionSource::get`] result unchanged.
    async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    /// Read the canonical [`crate::SessionQueries::agent_unavailable`] result.
    fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool;

    /// Read the selected [`awaken_executable_agent_contract::ExecutableAgentProfileSource::unavailable_delegate_in`] result unchanged.
    fn deployment_unavailable_delegate(&self, workspace_id: &str, agent_id: &str)
    -> Option<String>;
}

#[async_trait]
impl SessionTermination for crate::SessionTerminationCommands {
    async fn archive_session(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        crate::SessionTerminationCommands::archive_session(self, session_id).await
    }

    async fn release_profiled_session(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        crate::SessionTerminationCommands::release_profiled_session_with_verified_cleanup(
            self, session_id,
        )
        .await
    }

    async fn delete_session(
        self: Arc<Self>,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        crate::SessionTerminationCommands::delete_session(&self, command).await
    }

    async fn archive_with_repository_publication(
        &self,
        command: SessionArchiveWithRepositoryPublicationCommand,
    ) -> Result<
        SessionArchiveWithRepositoryPublicationOutcome,
        SessionArchiveWithRepositoryPublicationError,
    > {
        Box::pin(
            crate::SessionTerminationCommands::archive_with_repository_publication(self, command),
        )
        .await
    }

    async fn archive_session_thread(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        crate::SessionTerminationCommands::archive_session_thread(self, session_id, thread_id).await
    }
}

#[async_trait]
impl SessionUpdates for crate::SessionUpdateCommands {
    async fn update_session(
        &self,
        session_id: &SessionId,
        command: SessionUpdateCommand,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        Box::pin(crate::SessionUpdateCommands::update_session(
            self, session_id, command,
        ))
        .await
    }
}

#[async_trait]
impl SessionResourceCommands for crate::SessionResourceManifests {
    async fn replace_session_resource_manifest(
        &self,
        session_id: &SessionId,
        request: SessionResourceManifestRequest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        Box::pin(
            crate::SessionResourceManifests::replace_session_resource_manifest_request(
                self, session_id, request,
            ),
        )
        .await
    }

    async fn attach_session_file(
        &self,
        session_id: &SessionId,
        request: crate::SessionInputRequest,
    ) -> Result<SessionResourceAttachment, SessionResourceManifestError> {
        Box::pin(crate::SessionResourceManifests::attach_session_file(
            self, session_id, request,
        ))
        .await
    }

    async fn detach_session_input(
        &self,
        session_id: &SessionId,
        binding_id: &awaken_resource_contract::BindingId,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        Box::pin(crate::SessionResourceManifests::detach_session_input(
            self, session_id, binding_id,
        ))
        .await
    }
}

/// Session credential-binding commands consumed by Managed bind and Resource update.
///
/// O11 capability table: the one Managed credential-binding role exposes both
/// original commands. Creation and Resource roles no longer grant them, and
/// holding the application does not restore the removed business entrypoints.
/// These compile checks prove the source boundary; the existing real bind and
/// rotation cases exercise the same commands through the directly held service.
///
/// ```
/// use awaken_credential_contract::{CredentialMaterialInput};
/// use awaken_session_application::{SessionCredentialCommands};
/// use awaken_session_types::{SessionId};
/// use awaken_resource_contract::BindingId;
/// async fn binding_commands(role: &dyn SessionCredentialCommands, material: CredentialMaterialInput) {
///     let _ = role.missing_vault("workspace", &[]).await;
///     let _ = role.rotate_repository_credential(
///         &SessionId::from("session"), "workspace", &BindingId::from("binding"), material,
///     ).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionCreation;
/// async fn old_create_role(role: &dyn SessionCreation) {
///     let _ = role.missing_vault("workspace", &[]).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_credential_contract::{CredentialMaterialInput};
/// use awaken_session_application::{SessionResourceCommands};
/// use awaken_session_types::{SessionId};
/// use awaken_resource_contract::BindingId;
/// async fn old_resource_role(role: &dyn SessionResourceCommands, material: CredentialMaterialInput) {
///     let _ = role.rotate_repository_credential(
///         &SessionId::from("session"), "workspace", &BindingId::from("binding"), material,
///     ).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// async fn old_application(application: &SessionApplication) {
///     let _ = application.missing_vault("workspace", &[]).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_credential_contract::{CredentialMaterialInput};
/// use awaken_session_application::{SessionApplication};
/// use awaken_session_types::{SessionId};
/// use awaken_resource_contract::BindingId;
/// async fn old_application(application: &SessionApplication, material: CredentialMaterialInput) {
///     let _ = application.rotate_repository_credential(
///         &SessionId::from("session"), "workspace", &BindingId::from("binding"), material,
///     ).await;
/// }
/// ```
#[async_trait]
pub trait SessionCredentialCommands: Send + Sync {
    /// The first referenced Vault that does not exist, if any.
    async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError>;

    /// See [`crate::SessionCredentials::rotate_repository_credential`].
    async fn rotate_repository_credential(
        &self,
        session_id: &SessionId,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError>;
}

#[async_trait]
impl SessionCredentialCommands for crate::SessionCredentials {
    async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError> {
        crate::SessionCredentials::missing_vault(self, workspace_id, vault_ids).await
    }

    async fn rotate_repository_credential(
        &self,
        session_id: &SessionId,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        Box::pin(crate::SessionCredentials::rotate_repository_credential(
            self,
            session_id,
            owner_scope,
            binding_id,
            material,
        ))
        .await
    }
}
