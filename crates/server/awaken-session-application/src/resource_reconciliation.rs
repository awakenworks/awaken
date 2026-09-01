//! Durable Session Resource recovery and terminal reclamation.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceKind, ResourcePurgeError, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_session_contract::{
    ActivationState, IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    ResolvedInputSource, ResolvedSessionResources, RunError, SessionExecutionState,
    SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRealizationControlFailure, SessionRealizationLease, SessionResourceReferences,
    SessionRevision, SessionTombstone, stable_fingerprint,
};

use super::{
    ConfiguredSessionRepository, CredentialMaterialInput, CredentialMaterialRetirementCommand,
    CredentialMaterialRotationCommand, SessionApplication, SessionMutationError,
    SessionParticipantProvenance, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure, SessionRecoveryCandidates, SessionRepositoryOwner,
    mutation::repository_failure,
};

mod terminal_cleanup;

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedRepositoryRetirement {
    Retired,
    NotOwned,
    Pending,
}

impl OwnedRepositoryRetirement {
    const fn is_complete(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// Protocol-neutral whole-manifest command. The interface owns request
/// canonicalization; the Session application owns desired-state persistence,
/// idempotency, and realization ordering.
#[derive(Clone, Debug)]
pub struct ReplaceSessionResourceManifest {
    pub resources: ResolvedSessionResources,
    /// Optimistic fence for the Resource sub-aggregate, not the Session root.
    /// Runtime realization lease renewals advance the root revision without
    /// changing this generation and therefore must not starve Resource writes.
    pub expected_resource_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub request_fingerprint: String,
}

/// Durable command receipt plus the latest aggregate projection. The command
/// revision is stable across replay even if realization advances the root CAS.
#[derive(Clone, Debug)]
pub struct SessionResourceManifestOutcome {
    pub session: PersistedSession,
    pub command_revision: SessionRevision,
    pub command_applied: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionResourceManifestError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal")]
    Terminal,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session idempotency key was reused with another manifest")]
    IdempotencyMismatch,
    #[error("Session resource manifest was rejected: {0}")]
    Rejected(#[source] RunError),
    /// Desired truth already committed; a retry/reconciler may repair the
    /// disposable Runtime projection without accepting another generation.
    #[error("Session resource realization failed after commit: {source}")]
    ProjectionAfterCommit {
        outcome: Box<SessionResourceManifestOutcome>,
        #[source]
        source: SessionPreparationError,
    },
    #[error("Session resource persistence is unavailable: {0}")]
    Unavailable(String),
}

impl SessionResourceManifestError {
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

async fn resource_targets(
    files: &dyn FileCatalog,
    workspace: &str,
    resources: &SessionResourceReferences,
) -> Result<std::collections::BTreeSet<ResourceTarget>, ResourcePurgeError> {
    let mut targets = std::collections::BTreeSet::new();
    for input in resources.inputs() {
        let target = match &input.source {
            ResolvedInputSource::File { file_id } => ResourceTarget::new(
                workspace,
                ResourceKind::File,
                files
                    .get_file(workspace, file_id.as_str(), true)
                    .await
                    .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
                    .ok_or_else(|| {
                        ResourcePurgeError::Invalid(format!(
                            "Session references missing File `{file_id}`"
                        ))
                    })?
                    .blob_id,
            ),
            ResolvedInputSource::MemoryStore {
                memory_store_id, ..
            } => ResourceTarget::new(
                workspace,
                ResourceKind::MemoryStore,
                memory_store_id.as_str(),
            ),
            ResolvedInputSource::Repository { repository_id, .. } => {
                ResourceTarget::new(workspace, ResourceKind::Repository, repository_id.as_str())
            }
        };
        targets.insert(target);
    }
    targets.extend(
        resources
            .skills()
            .iter()
            .filter(|skill| skill.kind == awaken_agent_contract::AgentSkillKind::Custom)
            .map(|skill| ResourceTarget::new(workspace, ResourceKind::Skill, &skill.skill_id)),
    );
    Ok(targets)
}

/// Read-only reclamation guard over canonical Session aggregates. The durable
/// reference index closes mutation races; this independent scan prevents a
/// not-yet-realized pending manifest from being mistaken for unused data.
pub struct SessionResourcePurgeGuard {
    sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    files: Arc<dyn FileCatalog>,
}

impl SessionResourcePurgeGuard {
    #[must_use]
    pub fn new(
        sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
        files: Arc<dyn FileCatalog>,
    ) -> Self {
        Self { sessions, files }
    }
}

#[async_trait::async_trait]
impl ResourcePurgeGuard for SessionResourcePurgeGuard {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        _config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let mut blockers = std::collections::BTreeSet::new();
        let sessions = self
            .sessions
            .reconcilable_sessions()
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        if !sessions.quarantined.is_empty() {
            return Err(ResourcePurgeError::Storage(
                "Session recovery quarantine blocks physical Resource purge".to_string(),
            ));
        }
        for scoped in sessions.sessions {
            let workspace = scoped.workspace_id;
            let session = scoped.session;
            let references = session.resources.resource_references();
            let candidates = resource_targets(self.files.as_ref(), &workspace, &references).await?;
            let matches = candidates.iter().any(|candidate| {
                if target.kind == ResourceKind::File {
                    candidate.kind == ResourceKind::File
                        && candidate.resource_id == target.resource_id
                } else {
                    candidate == target
                }
            });
            if matches {
                blockers.insert(session.session_id);
            }
        }
        Ok(blockers
            .into_iter()
            .map(|session_id| ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: session_id,
            })
            .collect())
    }
}

pub(crate) fn mutation_failure(error: SessionMutationError) -> SessionPreparationError {
    match error {
        SessionMutationError::NotFound => SessionPreparationError::NotFound,
        SessionMutationError::Conflict => SessionPreparationError::Conflict,
        SessionMutationError::IdempotencyMismatch => SessionPreparationError::Unavailable(
            "Session idempotency key was reused with another payload".into(),
        ),
        SessionMutationError::Unavailable(message) => SessionPreparationError::Unavailable(message),
    }
}

fn repository_preparation(
    error: awaken_session_contract::SessionRepositoryError,
) -> SessionPreparationError {
    mutation_failure(repository_failure(error))
}

pub(crate) fn internal(error: impl std::fmt::Display) -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::internal(error.to_string()))
}

fn deletion_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .to_string()
}

impl SessionApplication {
    fn terminal_cleanup_lease_matches(
        session: &PersistedSession,
        asserted: &SessionRealizationLease,
    ) -> bool {
        session.realization.as_ref().is_some_and(|current| {
            current.owner == asserted.owner
                && current.runtime_incarnation == asserted.runtime_incarnation
                && current.epoch == asserted.epoch
                && current.expires_at_unix_ms >= asserted.expires_at_unix_ms
        })
    }

    /// Read the existing durable cleanup operation for its exact external
    /// realization owner. `Some([])` deliberately retains the Worker projection
    /// while the application is between Fence and frozen Requested intent.
    pub(crate) async fn external_terminal_cleanup_commands(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<Vec<awaken_session_contract::SessionCleanupCommand>>,
        SessionRealizationControlFailure,
    > {
        let session =
            self.session_repository()
                .get(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
        if !self.requires_external_realization(&session) {
            return Ok(None);
        }
        if !Self::terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        if session.terminal_cleanup.is_completed() {
            return Ok(None);
        }
        if session.is_terminal() || session.terminal_cleanup.needs_reconciliation() {
            return match session.terminal_cleanup.pending_commands(session_id) {
                Ok(commands) => {
                    let workspace_id = self.owner(session_id).await.map_err(|error| {
                        SessionRealizationControlFailure::Unavailable(error.to_string())
                    })?;
                    let restore_target = session
                        .environment
                        .restoring_request(&workspace_id, session_id);
                    let commands = commands
                        .into_iter()
                        .map(|command| match restore_target.clone() {
                            Some(request) if command.thread_id == session_id => {
                                command.with_restore_target(request).map_err(|error| {
                                    SessionRealizationControlFailure::Invalid(error.to_string())
                                })
                            }
                            _ => Ok(command),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Some(commands))
                }
                Err(awaken_session_contract::SessionCleanupError::NotRequested) => {
                    Ok(Some(Vec::new()))
                }
                Err(error) => Err(SessionRealizationControlFailure::Invalid(error.to_string())),
            };
        }
        Ok(None)
    }

    /// Project the root-only Repository publication effect under the exact
    /// realization lease that already owns terminal cleanup. The command and
    /// immutable Workspace owner are read from the Session authority on every
    /// poll; no application queue or receipt registry exists beside
    /// `SessionCleanupOperation`.
    pub(crate) async fn external_terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        let session =
            self.session_repository()
                .get(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
        if !self.requires_external_realization(&session) {
            return Ok(None);
        }
        if !Self::terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        if session.terminal_cleanup.is_completed() || !session.terminal_cleanup.is_requested() {
            return Ok(None);
        }
        let command = session
            .terminal_cleanup
            .publication_command(session_id)
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let Some(command) = command else {
            return Ok(None);
        };
        let workspace_id = self.owner(session_id).await.map_err(|error| match error {
            SessionMutationError::NotFound => SessionRealizationControlFailure::NotFound,
            error => SessionRealizationControlFailure::Unavailable(error.to_string()),
        })?;
        Ok(Some(
            awaken_session_contract::SessionRepositoryPublicationProjection {
                workspace_id,
                command,
            },
        ))
    }

    /// Verify and persist one exact Worker Repository publication receipt in
    /// the same root CAS as every cleanup completion. Root teardown remains a
    /// later command, so response loss can replay this receipt without losing
    /// the working tree that produced it.
    pub(crate) async fn record_external_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(|error| {
                SessionRealizationControlFailure::Unavailable(error.to_string())
            })?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
            if !self.requires_external_realization(&session) || !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not owned by an external terminal realization".into(),
                ));
            }
            if !Self::terminal_cleanup_lease_matches(&session, lease) {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            let changed = session
                .terminal_cleanup
                .record_repository_publication_receipt(session_id, receipt.clone())
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if changed {
                match self
                    .commit_resource_snapshot(
                        &owner_scope,
                        session,
                        "terminal-repository-publication-worker-receipt",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(SessionMutationError::Conflict)
                        if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                    {
                        continue;
                    }
                    Err(SessionMutationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(error) => {
                        return Err(SessionRealizationControlFailure::Unavailable(
                            error.to_string(),
                        ));
                    }
                }
            }
            self.wake_lifecycle_supervisor();
            return Ok(());
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Verify and persist one Worker completion through the same Session root
    /// CAS used by local cleanup, then let the canonical release driver finish
    /// only after the complete frozen target set has durable evidence.
    pub(crate) async fn record_external_terminal_cleanup_completion(
        &self,
        lease: &SessionRealizationLease,
        completion: awaken_session_contract::SessionCleanupCompletion,
    ) -> Result<(), SessionRealizationControlFailure> {
        let session_id = completion.session_id.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(&session_id).await.map_err(|error| {
                SessionRealizationControlFailure::Unavailable(error.to_string())
            })?;
            let mut session =
                self.session_repository()
                    .get(&session_id)
                    .await
                    .map_err(|error| match error {
                        awaken_session_contract::SessionRepositoryError::NotFound => {
                            SessionRealizationControlFailure::NotFound
                        }
                        error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                    })?;
            if !self.requires_external_realization(&session) || !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not owned by an external terminal realization".into(),
                ));
            }
            if !Self::terminal_cleanup_lease_matches(&session, lease) {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            let restore_target = session
                .environment
                .restoring_request(&owner_scope, &session_id)
                .filter(|_| completion.thread_id == session_id);
            let command = awaken_session_contract::SessionCleanupCommand {
                session_id: completion.session_id.clone(),
                thread_id: completion.thread_id.clone(),
                effect_id: completion.effect_id.clone(),
                restore_target,
            };
            let completion = completion
                .clone()
                .into_aggregate_completion(&command)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            let changed = session
                .record_terminal_cleanup_completion(completion.clone())
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if changed {
                session = match self
                    .commit_resource_snapshot(
                        &owner_scope,
                        session,
                        "terminal-cleanup-worker-receipt",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(session) => session,
                    Err(SessionMutationError::Conflict)
                        if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                    {
                        continue;
                    }
                    Err(SessionMutationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(error) => {
                        return Err(SessionRealizationControlFailure::Unavailable(
                            error.to_string(),
                        ));
                    }
                };
            }
            let no_cleanup_command = session
                .terminal_cleanup
                .pending_commands(&session_id)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
                .is_empty();
            let publication_pending = session
                .terminal_cleanup
                .publication_command(&session_id)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
                .is_some();
            if no_cleanup_command && !publication_pending {
                self.release_terminal_resources(&owner_scope, &session_id)
                    .await
                    .map_err(|error| {
                        SessionRealizationControlFailure::Unavailable(error.to_string())
                    })?;
            }
            self.wake_lifecycle_supervisor();
            return Ok(());
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn synchronize_resource_references(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let (Some(references), Some(files)) = (&self.resource_references, &self.resource_files)
        else {
            return Ok(());
        };
        let resource_pins = session.resources.resource_references();
        let targets = resource_targets(files.as_ref(), owner_scope, &resource_pins)
            .await
            .map_err(internal)?;
        let records = targets
            .into_iter()
            .map(|target| ResourceReferenceRecord {
                target,
                reference: ResourceReference {
                    kind: ResourceReferenceKind::SessionBinding,
                    reference_id: session.session_id.clone(),
                },
            })
            .collect();
        references
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                &session.session_id,
                records,
            )
            .await
            .map_err(internal)
    }

    /// Publish the candidate's complete retention set before its root CAS. A
    /// CAS failure repairs the projection from repository truth. This ordering
    /// makes additions fail closed against reclamation; removals remain safe
    /// because [`SessionResourcePurgeGuard`] still observes the pre-CAS aggregate.
    pub(crate) async fn commit_resource_snapshot(
        &self,
        owner_scope: &str,
        candidate: PersistedSession,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, SessionMutationError> {
        let session_id = candidate.session_id.clone();
        self.synchronize_resource_references(owner_scope, &candidate)
            .await
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
        match self
            .commit_session_snapshot(owner_scope, candidate, operation, lifecycle_facts)
            .await
        {
            Ok(committed) => Ok(committed),
            Err(error) => {
                self.repair_resource_references(owner_scope, &session_id)
                    .await;
                Err(error)
            }
        }
    }

    async fn commit_resource_snapshot_with_record(
        &self,
        owner_scope: &str,
        candidate: PersistedSession,
        idempotency: IdempotencyRecord,
    ) -> Result<(PersistedSession, bool), SessionMutationError> {
        let session_id = candidate.session_id.clone();
        self.synchronize_resource_references(owner_scope, &candidate)
            .await
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
        match self
            .commit_session_snapshot_with_record(owner_scope, candidate, idempotency, Vec::new())
            .await
        {
            Ok(committed) => Ok(committed),
            Err(error) => {
                self.repair_resource_references(owner_scope, &session_id)
                    .await;
                Err(error)
            }
        }
    }

    pub(crate) async fn repair_resource_references(&self, owner_scope: &str, session_id: &str) {
        let authoritative = self.session_repository().get(session_id).await;
        let repair = match authoritative {
            Ok(session) => {
                self.synchronize_resource_references(owner_scope, &session)
                    .await
            }
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                match &self.resource_references {
                    Some(references) => references
                        .replace_references(
                            ResourceReferenceKind::SessionBinding,
                            session_id,
                            Vec::new(),
                        )
                        .await
                        .map_err(internal),
                    None => Ok(()),
                }
            }
            Err(error) => Err(repository_preparation(error)),
        };
        if let Err(error) = repair {
            tracing::warn!(
                session_id,
                workspace_id = owner_scope,
                error = %error,
                "Session Resource reference repair remains pending"
            );
        }
    }

    #[must_use]
    pub fn resource_manifest_operation_id(session_id: &str, idempotency_key: &str) -> String {
        stable_fingerprint(&(session_id, idempotency_key))
    }

    fn resource_manifest_idempotency_record(
        session_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> IdempotencyRecord {
        IdempotencyRecord {
            key: format!(
                "managed:resource-manifest-command:{session_id}:{}",
                Self::resource_manifest_operation_id(session_id, idempotency_key)
            ),
            payload_hash: request_fingerprint.to_string(),
        }
    }

    /// Resolve an exact command replay before an interface performs lowering
    /// side effects such as Repository/Vault configuration. The main command
    /// repeats this check to close the race with a concurrent first writer.
    pub async fn replay_session_resource_manifest(
        &self,
        session_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError> {
        let record = Self::resource_manifest_idempotency_record(
            session_id,
            idempotency_key,
            request_fingerprint,
        );
        let Some(receipt) = self
            .session_repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?
        else {
            return Ok(None);
        };
        if receipt.payload_hash != record.payload_hash {
            return Err(SessionResourceManifestError::IdempotencyMismatch);
        }
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        self.converge_resource_manifest(SessionResourceManifestOutcome {
            session,
            command_revision: receipt.committed_revision,
            command_applied: false,
        })
        .await
        .map(Some)
    }

    /// Persist one complete desired Resource manifest through the Session root
    /// CAS, then ask the existing local reconciler or WorkQueue projection to
    /// realize that exact generation. No external effect runs before this
    /// method has a durable, queryable desired manifest.
    pub async fn replace_session_resource_manifest(
        &self,
        session_id: &str,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let command_record = command.idempotency_key.as_ref().map(|key| {
            Self::resource_manifest_idempotency_record(
                session_id,
                key,
                &command.request_fingerprint,
            )
        });

        if let Some(key) = &command.idempotency_key
            && let Some(outcome) = self
                .replay_session_resource_manifest(session_id, key, &command.request_fingerprint)
                .await?
        {
            return Ok(outcome);
        }

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .replace_session_resource_manifest_once(
                    session_id,
                    &command,
                    command_record.clone(),
                )
                .await
            {
                Err(SessionResourceManifestError::Conflict)
                    if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Ok(outcome) => return self.converge_resource_manifest(outcome).await,
                result => return result,
            }
        }
        Err(SessionResourceManifestError::Conflict)
    }

    async fn replace_session_resource_manifest_once(
        &self,
        session_id: &str,
        command: &ReplaceSessionResourceManifest,
        command_record: Option<IdempotencyRecord>,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let mut session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        if session.is_terminal() {
            return Err(SessionResourceManifestError::Terminal);
        }
        if command
            .expected_resource_revision
            .is_some_and(|expected| expected != session.resources.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
        }
        if let Some(baseline) = session.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_resource_replacement(session.resources.desired(), &command.resources)
        {
            return Err(SessionResourceManifestError::Rejected(
                RunError::bad_request(
                    "profiled Session resource mutation is outside its immutable policy",
                ),
            ));
        }

        let unchanged = session.resources.desired() == &command.resources;
        if !unchanged {
            let result = if session.resources.pending.is_some() {
                session
                    .resources
                    .revise_unattempted_pending(&session.session_id, command.resources.clone())
            } else {
                session
                    .resources
                    .prepare(&session.session_id, command.resources.clone())
            };
            result.map_err(|error| match error {
                awaken_session_contract::ResourceActivationError::Pending => {
                    SessionResourceManifestError::Conflict
                }
                error => {
                    SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
                }
            })?;
        }

        let command_receipt_key = command_record.as_ref().map(|record| record.key.clone());
        let (session, command_applied) = match command_record {
            Some(record) => self
                .commit_resource_snapshot_with_record(&owner_scope, session, record)
                .await
                .map_err(SessionResourceManifestError::mutation)?,
            None if unchanged => {
                let revision = session.revision;
                return Ok(SessionResourceManifestOutcome {
                    session,
                    command_revision: revision,
                    command_applied: false,
                });
            }
            None => (
                self.commit_resource_snapshot(
                    &owner_scope,
                    session,
                    "resource-manifest-intent",
                    Vec::new(),
                )
                .await
                .map_err(SessionResourceManifestError::mutation)?,
                true,
            ),
        };
        let command_revision = if command_applied {
            session.revision
        } else {
            let key = command_receipt_key.ok_or_else(|| {
                SessionResourceManifestError::Unavailable(
                    "replayed Resource manifest has no idempotency key".into(),
                )
            })?;
            self.session_repository()
                .idempotency_receipt(session_id, &key)
                .await
                .map_err(repository_failure)
                .map_err(SessionResourceManifestError::mutation)?
                .ok_or_else(|| {
                    SessionResourceManifestError::Unavailable(
                        "replayed Resource manifest has no idempotency receipt".into(),
                    )
                })?
                .committed_revision
        };
        Ok(SessionResourceManifestOutcome {
            session,
            command_revision,
            command_applied,
        })
    }

    async fn converge_resource_manifest(
        &self,
        mut outcome: SessionResourceManifestOutcome,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        if outcome.session.resources.pending.is_none()
            && !outcome.session.resources.has_repository_retirements()
        {
            return Ok(outcome);
        }
        let owner_scope = self
            .owner(&outcome.session.session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let convergence = if outcome.session.resources.pending.is_none() {
            self.reconcile_repository_retirements(&owner_scope, outcome.session.clone())
                .await
        } else if self.requires_external_realization(&outcome.session) {
            self.dispatch_session_work(&outcome.session)
                .await
                .map(|_| outcome.session.clone())
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))
        } else if outcome.session.execution == SessionExecutionState::Idle {
            self.reconcile_persisted_resources(&owner_scope, outcome.session.clone())
                .await
        } else {
            Ok(outcome.session.clone())
        };
        match convergence {
            Ok(session) => {
                outcome.session = session;
                Ok(outcome)
            }
            Err(source) => Err(SessionResourceManifestError::ProjectionAfterCommit {
                outcome: Box::new(outcome),
                source,
            }),
        }
    }

    /// Rotate and repin one Session-scoped Repository credential through the
    /// durable root CAS. A conflict reloads and recompiles the exact pin.
    pub async fn rotate_repository_credential(
        &self,
        session_id: &str,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
        material: CredentialMaterialInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let mut persisted = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_preparation)?;
        if let Some(baseline) = persisted.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_repository_credential_mutation()
        {
            return Err(SessionPreparationError::Rejected(RunError::bad_request(
                "profiled Session Repository credentials are immutable",
            )));
        }
        let ingress = self.credential_material_ingress().ok_or_else(|| {
            SessionPreparationError::Rejected(RunError::bad_request(
                "repository authorization requires a configured credential Vault",
            ))
        })?;
        let (credential_source, credential_revision, remote_url) = persisted
            .resources
            .active
            .inputs()
            .iter()
            .find(|input| input.binding_id == *binding_id)
            .and_then(|input| match &input.source {
                ResolvedInputSource::Repository {
                    config, credential, ..
                } => config
                    .credential_binding
                    .clone()
                    .zip(credential.as_ref())
                    .map(|(binding, credential)| {
                        (
                            binding,
                            credential.access.credential.revision,
                            config.remote_url.clone(),
                        )
                    }),
                _ => None,
            })
            .ok_or_else(|| {
                SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential update requires an authenticated Repository resource",
                ))
            })?;
        let credential_target =
            awaken_session_contract::repository_transport_credential_target(&remote_url).map_err(
                |error| SessionPreparationError::Rejected(RunError::bad_request(error.to_string())),
            )?;
        let completed_credential_revision = ingress
            .rotate_material(CredentialMaterialRotationCommand {
                source_id: awaken_credential_contract::CredentialSourceId(credential_source),
                expected_revision: credential_revision,
                workspace_id: owner_scope.to_owned(),
                target: credential_target,
                usage: awaken_session_contract::repository_transport_credential_usage(),
                material,
            })
            .await
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(format!(
                    "repository authorization could not be rotated: {error}"
                )))
            })?;

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let holder = self.resource_plaintext_holder(&persisted)?;
            let mut input = persisted
                .resources
                .active
                .inputs()
                .iter()
                .find(|input| input.binding_id == *binding_id)
                .cloned()
                .ok_or(SessionPreparationError::NotFound)?;
            let ResolvedInputSource::Repository { credential, .. } = &mut input.source else {
                return Err(SessionPreparationError::NotFound);
            };
            *credential = None;
            self.pin_repository_credential(owner_scope, &holder, &mut input)
                .await?;
            let ResolvedInputSource::Repository {
                credential: Some(credential),
                ..
            } = &input.source
            else {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential rotation did not produce an exact execution pin",
                )));
            };
            if credential.access.credential.revision != completed_credential_revision {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential revision changed before Session repin",
                )));
            }
            persisted.resources.active = persisted
                .resources
                .active
                .replace(input)
                .map_err(internal)?;
            match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "repository-credential-update",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    persisted = self
                        .session_repository()
                        .get(session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Settle the one canonical reconciliation attempt against its exact
    /// Resource generation. Root-only concurrent mutations are rebased; a
    /// competing Resource generation is never merged.
    async fn settle_resource_reconciliation(
        &self,
        owner_scope: &str,
        session_id: &str,
        resource_revision: u64,
        desired: &ResolvedSessionResources,
        settlement: ResourceSettlement,
    ) -> Result<PersistedSession, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut current = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_preparation)?;
            if current.resources.revision != resource_revision
                || current.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let operation = match &settlement {
                ResourceSettlement::Commit => {
                    current.resources.commit().map_err(internal)?;
                    "resource-reconcile-active"
                }
                ResourceSettlement::Rollback(error) => {
                    current
                        .resources
                        .rollback(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-rollback"
                }
                ResourceSettlement::RetryableFailure(error) => {
                    current
                        .resources
                        .note_retryable_failure(error.clone())
                        .map_err(internal)?;
                    "resource-reconcile-failed"
                }
            };
            match self
                .commit_resource_snapshot(owner_scope, current, operation, Vec::new())
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                result => return result.map_err(mutation_failure),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    async fn start_resource_reconciliation(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        desired: &ResolvedSessionResources,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let resource_revision = session.resources.revision;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if session.resources.revision != resource_revision
                || session.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let mut candidate = session.clone();
            candidate.resources.start_attempt().map_err(internal)?;
            match self
                .commit_resource_snapshot(
                    owner_scope,
                    candidate,
                    "resource-reconcile-attempt",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    session = self
                        .session_repository()
                        .get(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Commit the terminal delete tombstone through the canonical Session CAS.
    /// The `session.deleted` lifecycle fact belongs to the earlier atomic Delete
    /// fence; emitting it again here could duplicate delivery after an outbox
    /// consumer acknowledged the first fact.
    async fn commit_delete_tombstone(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionMutationError> {
        let deleted_revision =
            SessionRevision(session.revision.0.checked_add(1).ok_or_else(|| {
                SessionMutationError::Unavailable("Session revision exhausted".into())
            })?);
        let payload = SessionMutationPayload::Delete(SessionTombstone {
            session_id: session.session_id.clone(),
            deleted_revision,
            deleted_at: deletion_timestamp(),
        });
        let payload_hash = payload.stable_hash();
        let mutation = SessionMutation {
            expected_revision: session.revision,
            idempotency: awaken_session_contract::IdempotencyRecord {
                key: format!(
                    "session:delete:{}:{}:{payload_hash}",
                    session.session_id, session.revision.0
                ),
                payload_hash,
            },
            payload,
            lifecycle_facts: Vec::new(),
        };
        match self.commit_mutation(owner_scope, mutation).await? {
            SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                Ok(())
            }
            SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
            SessionMutationResult::IdempotencyMismatch => {
                Err(SessionMutationError::IdempotencyMismatch)
            }
        }
    }

    /// Reconcile every local durable Resource projection requiring convergence.
    pub async fn reconcile_resource_activations(&self) -> SessionReconciliation {
        let candidates = match self.session_repository().reconcilable_sessions().await {
            Ok(scan) => SessionRecoveryCandidates::from(scan),
            Err(error) => {
                let mut report = SessionReconciliation::default();
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        self.reconcile_resource_activations_from(&candidates).await
    }

    pub(super) async fn reconcile_resource_activations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
    ) -> SessionReconciliation {
        let mut report = SessionReconciliation {
            pending: candidates.sessions.len(),
            quarantined: candidates.quarantined.clone(),
            ..Default::default()
        };
        for candidate in &candidates.sessions {
            let owner_scope = &candidate.workspace_id;
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report.failures.push(SessionReconciliationFailure {
                        session_id: candidate.session_id.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let session_id = session.session_id.clone();
            if let Err(error) = self
                .synchronize_resource_references(owner_scope, &session)
                .await
            {
                report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                });
                continue;
            }
            if (!session.is_terminal()
                && self.requires_external_realization(&session)
                && !session.resources.has_repository_retirements())
                || !session.needs_resource_reconciliation()
            {
                continue;
            }
            match self
                .reconcile_persisted_resources(owner_scope, session)
                .await
            {
                Ok(session) => report.settled.push(session),
                Err(SessionPreparationError::NotFound) => {
                    // An eager terminal-cleanup actor may commit the delete
                    // tombstone after this scan captured its candidate. The
                    // missing aggregate is then the authoritative successful
                    // outcome, not a retryable reconciliation failure. Repair
                    // the disposable reference projection from that truth.
                    self.repair_resource_references(owner_scope, &session_id)
                        .await;
                }
                Err(error) => report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                }),
            }
        }
        report
    }

    /// Reconcile one persisted Session's Resource projection and reclamation.
    pub async fn reconcile_persisted_resources(
        &self,
        owner_scope: &str,
        session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let session = self
            .ensure_repository_credentials_pinned(owner_scope, session)
            .await?;
        if !session.is_terminal() && !self.requires_external_realization(&session) {
            if session.execution != SessionExecutionState::Idle {
                // Resource changes remain deferred while a Run owns execution.
                // Select that existing rule before constructing the complete
                // activation/retirement future on the supervisor poll stack.
                return Ok(session);
            }
            if !session.resources.needs_reconciliation()
                && session.resources.active.inputs().is_empty()
                && session.resources.activations.is_empty()
            {
                // This is the exact no-op branch below, selected before constructing
                // the full activation/retirement future. `needs_reconciliation` is
                // the canonical pending/activation/retirement-work predicate.
                return Ok(session);
            }
        }
        self.reconcile_prepared_resources(owner_scope, session)
            .await
    }

    /// Apply the Resource state machine only after credential migration has
    /// reached a stable Session root. Keeping these phases as separate futures
    /// also bounds synchronous poll-stack growth on the HTTP admission path.
    async fn reconcile_prepared_resources(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        if !session.is_terminal() && self.requires_external_realization(&session) {
            return if session.resources.pending.is_none()
                && session.resources.has_repository_retirements()
            {
                self.reconcile_repository_retirements(owner_scope, session)
                    .await
            } else {
                Ok(session)
            };
        }
        let session_id = session.session_id.clone();
        if session.execution == SessionExecutionState::Idle && !session.is_terminal() {
            if let Some(desired) = session.resources.pending.clone() {
                let previous = session.resources.active.clone();
                let previous_revision = session.resources.active_revision();
                session = self
                    .start_resource_reconciliation(owner_scope, session, &desired)
                    .await?;
                if let Err(error) = self
                    .runtime()
                    .apply_session_inputs(
                        &session_id,
                        owner_scope,
                        session.resources.revision,
                        &desired,
                    )
                    .await
                {
                    let settlement = match self
                        .runtime()
                        .apply_session_inputs(
                            &session_id,
                            owner_scope,
                            previous_revision,
                            &previous,
                        )
                        .await
                    {
                        Ok(()) => ResourceSettlement::Rollback(error.to_string()),
                        Err(rollback_error) => ResourceSettlement::RetryableFailure(format!(
                            "activation failed: {error}; rollback failed: {rollback_error}"
                        )),
                    };
                    self.settle_resource_reconciliation(
                        owner_scope,
                        &session_id,
                        session.resources.revision,
                        &desired,
                        settlement,
                    )
                    .await?;
                    return Err(SessionPreparationError::Rejected(error));
                }
                let committed = self
                    .settle_resource_reconciliation(
                        owner_scope,
                        &session_id,
                        session.resources.revision,
                        &desired,
                        ResourceSettlement::Commit,
                    )
                    .await?;
                return if committed.resources.has_repository_retirements() {
                    self.reconcile_repository_retirements(owner_scope, committed)
                        .await
                } else {
                    Ok(committed)
                };
            }

            if session
                .resources
                .activations
                .iter()
                .any(|activation| activation.state == ActivationState::Releasing)
            {
                return Err(internal(
                    "resource activation has Releasing records without a pending manifest",
                ));
            }
            if session.resources.active.inputs().is_empty()
                && session.resources.activations.is_empty()
            {
                return if session.resources.has_repository_retirements() {
                    self.reconcile_repository_retirements(owner_scope, session)
                        .await
                } else {
                    Ok(session)
                };
            }
            let (active_revision, active_resources) = session.resources.active_generation();
            self.runtime()
                .apply_session_inputs(&session_id, owner_scope, active_revision, active_resources)
                .await
                .map_err(SessionPreparationError::Rejected)?;
            if session.resources.activations.is_empty() {
                session.resources.adopt_legacy_active(&session_id);
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-adopt-legacy",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
            }
            return if session.resources.has_repository_retirements() {
                self.reconcile_repository_retirements(owner_scope, session)
                    .await
            } else {
                Ok(session)
            };
        }

        match self
            .release_terminal_resources(owner_scope, &session_id)
            .await?
        {
            Some(session) => Ok(session),
            None => Ok(session),
        }
    }

    async fn reconcile_repository_retirements(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        loop {
            let Some(retirement) = session.resources.repository_retirements().first().cloned()
            else {
                return Ok(session);
            };
            let ResolvedInputSource::Repository { repository_id, .. } = &retirement.source else {
                return Err(internal(
                    "Session Repository retirement intent contains another Resource kind",
                ));
            };
            if session_resources_reference_repository_generation(
                &session.resources.active,
                repository_id.as_str(),
            ) {
                let mut candidate = session.clone();
                candidate
                    .resources
                    .complete_repository_retirement(&retirement);
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        candidate,
                        "repository-retirement-cancelled",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
                continue;
            }
            if session.resources.pending.as_ref().is_some_and(|pending| {
                session_resources_reference_repository_generation(pending, repository_id.as_str())
            }) {
                return Ok(session);
            }
            if !self
                .retire_session_repository_input(
                    owner_scope,
                    &session.session_id,
                    &retirement.source,
                )
                .await
            {
                return Err(internal(
                    "Session-scoped Repository cleanup remains pending",
                ));
            }
            let mut candidate = session.clone();
            if !candidate
                .resources
                .complete_repository_retirement(&retirement)
            {
                return Err(internal(
                    "Session Repository retirement intent disappeared before completion",
                ));
            }
            match self
                .commit_resource_snapshot(
                    owner_scope,
                    candidate,
                    "repository-retirement-complete",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => session = committed,
                Err(SessionMutationError::Conflict) => {
                    session = self
                        .session_repository()
                        .get(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
    }

    pub async fn retire_session_repositories(
        &self,
        owner_scope: &str,
        session_id: &str,
        resources: &awaken_session_contract::SessionResourceState,
    ) -> bool {
        let mut repositories = std::collections::BTreeMap::new();
        for input in std::iter::once(&resources.active)
            .chain(resources.pending.iter())
            .flat_map(awaken_session_contract::ResolvedSessionResources::inputs)
            .chain(resources.repository_retirements().iter())
        {
            let ResolvedInputSource::Repository { repository_id, .. } = &input.source else {
                continue;
            };
            repositories
                .entry(repository_id.to_string())
                .and_modify(|entry: &mut ResolvedInputSource| {
                    if repository_source_credential_revision(&input.source)
                        > repository_source_credential_revision(entry)
                    {
                        entry.clone_from(&input.source);
                    }
                })
                .or_insert_with(|| input.source.clone());
        }
        let mut retired = true;
        for source in repositories.into_values() {
            retired &= self
                .retire_session_repository_input(owner_scope, session_id, &source)
                .await;
        }
        retired
    }

    /// Compensate only participants created by a command that failed before a
    /// durable Session root adopted them. Exact replays belong to an earlier
    /// command and are never inferred to be disposable.
    pub async fn abort_unadopted_session_repositories(
        &self,
        configured: &[ConfiguredSessionRepository],
    ) -> bool {
        let mut retired = true;
        for configured in configured.iter().rev() {
            let adopted = match self
                .session_repository()
                .get(configured.owner.session_id())
                .await
            {
                Ok(session) => session_resources_reference_repository(
                    &session.resources,
                    configured.repository_id.as_str(),
                ),
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => false,
                Err(_) => {
                    // An unavailable/corrupt root read cannot prove that a
                    // concurrent exact command did not adopt this participant.
                    retired = false;
                    continue;
                }
            };
            if adopted {
                continue;
            }
            if configured.registry_provenance == SessionParticipantProvenance::Applied {
                retired &= self
                    .retire_owned_repository(
                        &configured.workspace_id,
                        &configured.owner,
                        configured.repository_id.as_str(),
                    )
                    .await;
            }
            if let Some(credential) = &configured.credential
                && credential.provenance == SessionParticipantProvenance::Applied
            {
                retired &= self
                    .retire_owned_repository_credential(
                        &configured.workspace_id,
                        &credential.credential,
                    )
                    .await;
            }
        }
        retired
    }

    /// Retire one detached Repository input only when both its canonical
    /// Session namespace and durable owner marker agree. Platform/shared
    /// Repository inputs are intentionally a no-op.
    pub async fn retire_session_repository_input(
        &self,
        owner_scope: &str,
        session_id: &str,
        source: &ResolvedInputSource,
    ) -> bool {
        let ResolvedInputSource::Repository {
            repository_id,
            config,
            credential,
        } = source
        else {
            return true;
        };
        let Some(owner) = session_repository_owner(session_id, repository_id.as_str()) else {
            return true;
        };
        let owned_credential_binding = format!("{}:credential", repository_id.as_str());
        let owned_credential = credential
            .as_deref()
            .filter(|_| config.credential_binding.as_deref() == Some(&owned_credential_binding))
            .map(|credential| &credential.access.credential);
        self.retire_owned_repository_participants(
            owner_scope,
            &owner,
            repository_id.as_str(),
            owned_credential,
        )
        .await
        .is_complete()
    }

    pub(crate) async fn retire_owned_repository(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
    ) -> bool {
        self.retire_owned_repository_participants(owner_scope, owner, repository_id, None)
            .await
            .is_complete()
    }

    async fn retire_owned_repository_participants(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
        credential: Option<&awaken_credential_contract::CredentialRef>,
    ) -> OwnedRepositoryRetirement {
        let Some(catalog) = self.resource_registry() else {
            return OwnedRepositoryRetirement::Pending;
        };
        let definition = match catalog.find_repository(owner_scope, repository_id) {
            Ok(Some(definition)) => definition,
            // The prior idempotent attempt may already have retired and purged
            // the Registry participant before its Session-root completion CAS.
            // Absence therefore completes without inferring credential authority.
            Ok(None) => return OwnedRepositoryRetirement::NotOwned,
            Err(_) => return OwnedRepositoryRetirement::Pending,
        };
        if !owner.matches_definition(&definition) {
            // Namespace resemblance alone never grants delete authority. This
            // is a successful no-op for platform/shared definitions so one
            // terminal Session cannot be held open by a resource it does not
            // own.
            return OwnedRepositoryRetirement::NotOwned;
        }
        if let Some(credential) = credential
            && !self
                .retire_owned_repository_credential(owner_scope, credential)
                .await
        {
            return OwnedRepositoryRetirement::Pending;
        }
        if self.retire_repository(owner_scope, repository_id).await {
            OwnedRepositoryRetirement::Retired
        } else {
            OwnedRepositoryRetirement::Pending
        }
    }

    async fn retire_owned_repository_credential(
        &self,
        owner_scope: &str,
        credential: &awaken_credential_contract::CredentialRef,
    ) -> bool {
        let Some(ingress) = self.credential_material_ingress() else {
            return false;
        };
        ingress
            .retire_material(CredentialMaterialRetirementCommand {
                credential: credential.clone(),
                workspace_id: owner_scope.to_owned(),
            })
            .await
            .is_ok()
    }

    pub async fn retire_repository(&self, owner_scope: &str, repository_id: &str) -> bool {
        let Some(catalog) = self.resource_registry() else {
            return true;
        };
        let definition = match catalog.find_repository(owner_scope, repository_id) {
            Ok(Some(definition)) => definition,
            Ok(None) => return true,
            Err(_) => return false,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        if let Some(scheduler) = self.resource_purge_scheduler()
            && scheduler
                .schedule_purge(
                    awaken_resource_contract::ResourceTarget::new(
                        owner_scope,
                        awaken_resource_contract::ResourceKind::Repository,
                        repository_id,
                    ),
                    Some(definition.current_config_version.0),
                    now,
                    now,
                )
                .await
                .is_err()
        {
            return false;
        }
        catalog
            .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
                workspace_id: owner_scope.into(),
                id: repository_id.into(),
                state: awaken_resource_contract::ResourceState::Deleted,
            })
            .is_ok()
    }
}

fn session_repository_owner(
    session_id: &str,
    repository_id: &str,
) -> Option<SessionRepositoryOwner> {
    [
        SessionRepositoryOwner::managed(session_id),
        SessionRepositoryOwner::profiled(session_id),
    ]
    .into_iter()
    .find(|owner| owner.owns_repository_id(repository_id))
}

fn session_resources_reference_repository(
    resources: &awaken_session_contract::SessionResourceState,
    repository_id: &str,
) -> bool {
    std::iter::once(&resources.active)
        .chain(resources.pending.iter())
        .flat_map(awaken_session_contract::ResolvedSessionResources::inputs)
        .chain(resources.repository_retirements().iter())
        .any(|input| {
            matches!(
                &input.source,
                ResolvedInputSource::Repository {
                    repository_id: candidate,
                    ..
                } if candidate.as_str() == repository_id
            )
        })
}

fn session_resources_reference_repository_generation(
    resources: &ResolvedSessionResources,
    repository_id: &str,
) -> bool {
    resources.inputs().iter().any(|input| {
        matches!(
            &input.source,
            ResolvedInputSource::Repository {
                repository_id: candidate,
                ..
            } if candidate.as_str() == repository_id
        )
    })
}

fn repository_source_credential_revision(source: &ResolvedInputSource) -> Option<u64> {
    match source {
        ResolvedInputSource::Repository { credential, .. } => credential
            .as_deref()
            .map(|credential| credential.access.credential.revision),
        ResolvedInputSource::File { .. } | ResolvedInputSource::MemoryStore { .. } => None,
    }
}
