// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-application/src/mutation.rs
// @formal owner crates/server/awaken-session-application/src/terminal.rs
// @formal owner crates/server/awaken-session-application/src/resource_reconciliation.rs
// @formal owner crates/server/awaken-session-application/src/credentials.rs
// @formal reason: Declares the consumer roles of SessionApplication and implements each by
// @formal reason: forwarding to the owning command or shared archive kernel; the root CAS, cleanup,
// @formal reason: Resource and credential decisions stay with the owners named above.
//! The Session application's roles, each cut to the adapter code that calls it.
//!
//! A protocol adapter holds the roles it calls instead of the whole
//! application, so what an adapter may do is visible in its fields and a role
//! can be supplied by another implementation without the rest. Each trait names
//! its consumers; a method belongs to exactly one role. The commands, outcomes
//! and errors these roles speak are owned by this crate, so the traits live
//! beside them rather than in a contract crate.
//!
//! [`SessionApplication`] is the one implementation. Every method forwards to
//! its owning command. Termination-only role lowering selects the existing
//! archive kernel without extending the broad inherent application surface.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_environment_contract::EnvItem;
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_application_contract::{ExecutableProjectionAdmission, SessionEventAdmission};
use awaken_session_budget_contract::SessionUsage;
use awaken_session_configuration_contract::AgentCapabilities;
use awaken_session_event_contract::{SessionEventBatch, SessionEventInput};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, CoordinatedThreadLink, SessionRunActivation,
    SessionThreadLiveSubscription,
};
use awaken_session_state_contract::{
    DeletedSessionEventAdmissions, IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    SessionRepositoryError, SessionScan, SessionScanQuery,
};
use awaken_session_types::RunError;
use awaken_session_types::SessionId;

use crate::{
    CommittedSessionFacts, ConfiguredSessionRepository, CreateProfiledSessionCommand,
    CreatedSession, CredentialMaterialInput, ProjectionThreadSnapshots, RecoveredSessionProjection,
    ReplaceSessionResourceManifest, SessionApplication,
    SessionArchiveWithRepositoryPublicationCommand, SessionArchiveWithRepositoryPublicationError,
    SessionArchiveWithRepositoryPublicationOutcome, SessionCreateProtocol, SessionCreateRequest,
    SessionCreateRequestError, SessionCreationError, SessionDeleteCommand,
    SessionDispositionMutation, SessionEventBatchIdempotency, SessionExecutionObservation,
    SessionMutationError, SessionPreparationError, SessionProjectionRecoveryError,
    SessionRepositoryResourceInput, SessionResourceManifestError, SessionResourceManifestOutcome,
    SessionUpdateCommand, SessionUpdateError, SessionUpdateOutcome,
};

/// Create a Session from a protocol-lowered request, and replay a create.
///
/// `Projection` is what the calling protocol finishes before the durable
/// insert (see [`SessionCreateProtocol::Projection`]).
///
/// Consumer: a protocol's Session create command, its create-retry replay and
/// its bind-time Vault check.
#[async_trait]
pub trait SessionCreation<Projection: Send + 'static>: Send + Sync {
    /// See [`SessionApplication::create_requested_session`].
    async fn create_requested_session(
        &self,
        request: SessionCreateRequest,
        protocol: &dyn SessionCreateProtocol<Projection = Projection>,
    ) -> Result<CreatedSession<Projection>, SessionCreateRequestError>;

    /// See [`SessionApplication::replay_session_create`].
    async fn replay_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError>;

    /// See [`SessionApplication::replay_accepted_session_create`].
    async fn replay_accepted_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError>;

    /// The first referenced Vault that does not exist, if any.
    async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError>;

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

    /// Every visible root one Workspace owns.
    async fn sessions_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError>;

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

/// Read the evidence a Session projection is built from.
///
/// Consumer: a protocol's committed projection refresh, cold rehydration and
/// Session projection reads.
#[async_trait]
pub trait SessionProjectionReads: Send + Sync {
    /// See [`SessionApplication::read_projection_evidence`].
    async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<CommittedSessionFacts, RunError>;

    /// See [`SessionApplication::projection_thread_snapshot`].
    async fn projection_thread_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError>;

    /// See [`SessionApplication::projection_coordinated_prefix`].
    async fn projection_coordinated_prefix(
        &self,
        session_id: &SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError>;

    /// See [`SessionApplication::read_session_projection`].
    async fn read_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError>;

    /// See [`SessionApplication::refresh_executable_projections`].
    async fn refresh_executable_projections(&self)
    -> Result<ExecutableProjectionAdmission, String>;

    /// The capabilities the Runtime serves on one Thread.
    fn capabilities_for(&self, thread: &str) -> AgentCapabilities;

    /// The current Session profile of one published Agent.
    fn session_profile(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile>;

    /// The Session profile of one Agent at an exact publication revision.
    fn session_profile_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSessionProfile>;
}

/// Admit Event batches into a Session.
///
/// Consumer: a protocol's Event append command and its admission-close retry.
#[async_trait]
pub trait SessionEventBatches: Send + Sync {
    /// See [`SessionApplication::append_and_drive_session_event_batch_idempotent`].
    async fn append_and_drive_session_event_batch_idempotent(
        &self,
        session_id: &SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(SessionEventBatch, Option<RunError>), RunError>;

    /// See [`SessionApplication::append_session_event_batch_idempotent`].
    async fn append_session_event_batch_idempotent(
        &self,
        session_id: &SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatch, RunError>;

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
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool;
}

/// Create and run product-profiled Sessions.
///
/// Consumer: the profiled Session create and Run edges.
#[async_trait]
pub trait ProfiledSessionCommands: Send + Sync {
    /// See [`SessionApplication::accept_profiled_session`].
    async fn accept_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError>;

    /// See [`SessionApplication::admit_session_run_for_owner`].
    async fn admit_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError>;

    /// See [`SessionApplication::activate_admitted_session_run`].
    async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<SessionRunActivation, RunError>;

    /// See [`SessionApplication::session_run_events_after`].
    async fn session_run_events_after(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
        dispatch_after: awaken_dispatch_contract::DispatchCursor,
        lifecycle_after: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_dispatch_contract::SessionRunEventPage, SessionProjectionRecoveryError>;

    /// See [`SessionApplication::reply_profiled_session_run_for_owner`].
    async fn reply_profiled_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), SessionProjectionRecoveryError>;
}

/// End a Session or one of its Threads.
///
/// Consumer: a protocol's archive, delete, archive-with-publication and Thread
/// archive commands.
#[async_trait]
pub trait SessionTermination: Send + Sync {
    /// See [`SessionApplication::terminate_session`].
    async fn terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError>;

    /// See [`SessionApplication::release_terminal_session`].
    async fn release_terminal_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError>;

    /// Release a never-attempted Frozen preparation through the shared archive kernel.
    async fn release_unattempted_profiled_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
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

/// Resolve, configure and replace a Session's Resources.
///
/// Consumer: a protocol's Session Resource commands and the input resolution
/// of its create and Deployment launch paths.
#[async_trait]
pub trait SessionResourceCommands: Send + Sync {
    /// See [`SessionApplication::abort_unadopted_session_repositories`].
    async fn abort_unadopted_session_repositories(
        &self,
        configured: &[ConfiguredSessionRepository],
    ) -> bool;

    /// Configure one Session-owned Repository participant.
    async fn configure_session_repository(
        &self,
        input: SessionRepositoryResourceInput,
    ) -> Result<ConfiguredSessionRepository, RunError>;

    /// See [`SessionApplication::replace_session_resource_manifest`].
    async fn replace_session_resource_manifest(
        &self,
        session_id: &SessionId,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError>;

    /// See [`SessionApplication::replay_session_resource_manifest`].
    async fn replay_session_resource_manifest(
        &self,
        session_id: &SessionId,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError>;

    /// Resolve Agent defaults and Session attachments into Resource bindings.
    async fn resolve_session_inputs(
        &self,
        workspace_id: &str,
        agent_defaults: &[awaken_resource_contract::InputBinding],
        session_inputs: &[awaken_session_resource_contract::SessionInputAttachment],
    ) -> Result<awaken_session_resource_contract::ResolvedSessionResources, RunError>;

    /// See [`SessionApplication::rotate_repository_credential`].
    async fn rotate_repository_credential(
        &self,
        session_id: &SessionId,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError>;

    /// The definition of one Memory store a Session may mount.
    async fn session_memory_store(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<awaken_resource_contract::MemoryStoreDefinition, RunError>;

    /// See [`SessionApplication::validate_persisted_session_resource_layout`].
    fn validate_persisted_session_resource_layout(
        &self,
        workspace_id: &str,
        session: &PersistedSession,
        resources: Vec<awaken_resource_contract::InputBinding>,
    ) -> Result<(), RunError>;
}

/// Observe a Session's live and committed execution.
///
/// Consumer: a protocol's live event stream, execution evidence read and Dream
/// usage read.
#[async_trait]
pub trait SessionObservation: Send + Sync {
    /// See [`SessionApplication::subscribe_session_thread_live`].
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
    /// See [`SessionApplication::deployment_environment`].
    async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    /// See [`SessionApplication::deployment_agent_unavailable`].
    fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool;

    /// See [`SessionApplication::deployment_unavailable_delegate`].
    fn deployment_unavailable_delegate(&self, workspace_id: &str, agent_id: &str)
    -> Option<String>;
}

#[async_trait]
impl<Projection: Send + 'static> SessionCreation<Projection> for SessionApplication {
    async fn create_requested_session(
        &self,
        request: SessionCreateRequest,
        protocol: &dyn SessionCreateProtocol<Projection = Projection>,
    ) -> Result<CreatedSession<Projection>, SessionCreateRequestError> {
        SessionApplication::create_requested_session(self, request, protocol).await
    }

    async fn replay_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        SessionApplication::replay_session_create(self, owner_scope, session_id, idempotency).await
    }

    async fn replay_accepted_session_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        SessionApplication::replay_accepted_session_create(
            self,
            owner_scope,
            session_id,
            idempotency,
        )
        .await
    }

    async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError> {
        SessionApplication::missing_vault(self, workspace_id, vault_ids).await
    }

    fn runtime_incarnation(&self) -> &str {
        SessionApplication::runtime_incarnation(self)
    }
}

#[async_trait]
impl SessionRootReads for SessionApplication {
    async fn session(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        SessionApplication::session(self, session_id).await
    }

    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionMutationError> {
        SessionApplication::owner(self, session_id).await
    }

    async fn sessions_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        SessionApplication::sessions_by_owner(self, owner_scope).await
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.sessions_repo
            .scan(SessionScanQuery::CredentialSourceReferences {
                workspace_id,
                source_id,
            })
            .await
            .map(SessionScan::into_sessions)
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError> {
        self.sessions_repo
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }
}

#[async_trait]
impl SessionProjectionReads for SessionApplication {
    async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<CommittedSessionFacts, RunError> {
        SessionApplication::read_projection_evidence(self, persisted, started).await
    }

    async fn projection_thread_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        SessionApplication::projection_thread_snapshot(self, session_id, thread_id).await
    }

    async fn projection_coordinated_prefix(
        &self,
        session_id: &SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError> {
        SessionApplication::projection_coordinated_prefix(self, session_id, observed_root).await
    }

    async fn read_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        SessionApplication::read_session_projection(self, thread_id, expected_owner).await
    }

    async fn refresh_executable_projections(
        &self,
    ) -> Result<ExecutableProjectionAdmission, String> {
        SessionApplication::refresh_executable_projections(self).await
    }

    fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        SessionApplication::capabilities_for(self, thread)
    }

    fn session_profile(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        SessionApplication::session_profile(self, workspace_id, agent_id)
    }

    fn session_profile_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        SessionApplication::session_profile_at_revision(
            self,
            workspace_id,
            agent_id,
            source_revision,
        )
    }
}

#[async_trait]
impl SessionEventBatches for SessionApplication {
    async fn append_and_drive_session_event_batch_idempotent(
        &self,
        session_id: &SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(SessionEventBatch, Option<RunError>), RunError> {
        SessionApplication::append_and_drive_session_event_batch_idempotent(
            self,
            session_id,
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    async fn append_session_event_batch_idempotent(
        &self,
        session_id: &SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatch, RunError> {
        SessionApplication::append_session_event_batch_idempotent(
            self,
            session_id,
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    async fn session_event_batch_idempotency(
        &self,
        session_id: &SessionId,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError> {
        SessionApplication::session_event_batch_idempotency(
            self,
            session_id,
            key,
            request_fingerprint,
        )
        .await
    }

    async fn close_session_event_admission(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        key: String,
        request_fingerprint: String,
    ) -> Result<SessionEventAdmission, RunError> {
        SessionApplication::close_session_event_admission(
            self,
            owner_scope,
            session_id,
            key,
            request_fingerprint,
        )
        .await
    }

    fn agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        SessionApplication::agent_unavailable(self, workspace_id, agent_id)
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        SessionApplication::supports_mid_conversation_system(self, thread).await
    }
}

#[async_trait]
impl ProfiledSessionCommands for SessionApplication {
    async fn accept_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError> {
        SessionApplication::accept_profiled_session(self, command).await
    }

    async fn admit_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        Box::pin(SessionApplication::admit_session_run_for_owner(
            self,
            owner_scope,
            command,
        ))
        .await
    }

    async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<SessionRunActivation, RunError> {
        Box::pin(SessionApplication::activate_admitted_session_run(
            self, admitted,
        ))
        .await
    }

    async fn session_run_events_after(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
        dispatch_after: awaken_dispatch_contract::DispatchCursor,
        lifecycle_after: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_dispatch_contract::SessionRunEventPage, SessionProjectionRecoveryError>
    {
        SessionApplication::session_run_events_after(
            self,
            owner_scope,
            session_id,
            run_id,
            dispatch_after,
            lifecycle_after,
            limit,
        )
        .await
    }

    async fn reply_profiled_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), SessionProjectionRecoveryError> {
        SessionApplication::reply_profiled_session_run_for_owner(self, owner_scope, command).await
    }
}

#[async_trait]
impl SessionTermination for SessionApplication {
    async fn terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        SessionApplication::terminate_session(self, session_id, archived_at, fact).await
    }

    async fn release_terminal_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        SessionApplication::release_terminal_session(self, session_id, archived_at, fact).await
    }

    async fn release_unattempted_profiled_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        crate::terminal::release_unattempted_profiled_session(self, session_id, archived_at, fact)
            .await
    }

    async fn delete_session(
        self: Arc<Self>,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        SessionApplication::delete_session(&self, command).await
    }

    async fn archive_with_repository_publication(
        &self,
        command: SessionArchiveWithRepositoryPublicationCommand,
    ) -> Result<
        SessionArchiveWithRepositoryPublicationOutcome,
        SessionArchiveWithRepositoryPublicationError,
    > {
        Box::pin(SessionApplication::archive_with_repository_publication(
            self, command,
        ))
        .await
    }

    async fn archive_session_thread(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        SessionApplication::archive_session_thread(self, session_id, thread_id).await
    }
}

#[async_trait]
impl SessionUpdates for SessionApplication {
    async fn update_session(
        &self,
        session_id: &SessionId,
        command: SessionUpdateCommand,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        Box::pin(SessionApplication::update_session(
            self, session_id, command,
        ))
        .await
    }
}

#[async_trait]
impl SessionResourceCommands for SessionApplication {
    async fn abort_unadopted_session_repositories(
        &self,
        configured: &[ConfiguredSessionRepository],
    ) -> bool {
        SessionApplication::abort_unadopted_session_repositories(self, configured).await
    }

    async fn configure_session_repository(
        &self,
        input: SessionRepositoryResourceInput,
    ) -> Result<ConfiguredSessionRepository, RunError> {
        SessionApplication::configure_session_repository(self, input).await
    }

    async fn replace_session_resource_manifest(
        &self,
        session_id: &SessionId,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        Box::pin(SessionApplication::replace_session_resource_manifest(
            self, session_id, command,
        ))
        .await
    }

    async fn replay_session_resource_manifest(
        &self,
        session_id: &SessionId,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError> {
        SessionApplication::replay_session_resource_manifest(
            self,
            session_id,
            idempotency_key,
            request_fingerprint,
        )
        .await
    }

    async fn resolve_session_inputs(
        &self,
        workspace_id: &str,
        agent_defaults: &[awaken_resource_contract::InputBinding],
        session_inputs: &[awaken_session_resource_contract::SessionInputAttachment],
    ) -> Result<awaken_session_resource_contract::ResolvedSessionResources, RunError> {
        SessionApplication::resolve_session_inputs(
            self,
            workspace_id,
            agent_defaults,
            session_inputs,
        )
        .await
    }

    async fn rotate_repository_credential(
        &self,
        session_id: &SessionId,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        Box::pin(SessionApplication::rotate_repository_credential(
            self,
            session_id,
            owner_scope,
            binding_id,
            material,
        ))
        .await
    }

    async fn session_memory_store(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<awaken_resource_contract::MemoryStoreDefinition, RunError> {
        SessionApplication::session_memory_store(self, workspace_id, memory_store_id).await
    }

    fn validate_persisted_session_resource_layout(
        &self,
        workspace_id: &str,
        session: &PersistedSession,
        resources: Vec<awaken_resource_contract::InputBinding>,
    ) -> Result<(), RunError> {
        SessionApplication::validate_persisted_session_resource_layout(
            self,
            workspace_id,
            session,
            resources,
        )
    }
}

#[async_trait]
impl SessionObservation for SessionApplication {
    async fn subscribe_session_thread_live(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError> {
        SessionApplication::subscribe_session_thread_live(self, session_id, thread_id).await
    }

    async fn session_execution_observation(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
    ) -> Result<SessionExecutionObservation, SessionProjectionRecoveryError> {
        SessionApplication::session_execution_observation(self, workspace_id, session_id).await
    }

    async fn session_usage(
        &self,
        thread: &str,
        observed_links: Option<&[CoordinatedThreadLink]>,
    ) -> Result<SessionUsage, RunError> {
        SessionApplication::session_usage(self, thread, observed_links).await
    }
}

#[async_trait]
impl DeploymentSessionEnvironment for SessionApplication {
    async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        SessionApplication::deployment_environment(self, environment_id).await
    }

    fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        SessionApplication::deployment_agent_unavailable(self, workspace_id, agent_id)
    }

    fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        SessionApplication::deployment_unavailable_delegate(self, workspace_id, agent_id)
    }
}
