//! Durable Session Resource recovery and terminal reclamation.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceKind, ResourcePurgeError, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_session_contract::{
    ActivationState, IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    ResolvedInputSource, ResolvedSessionResources, RunError, SessionExecutionState,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRevision,
    SessionTombstone, stable_fingerprint,
};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure, mutation::repository_failure,
};

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
}

/// Protocol-neutral whole-manifest command. The interface owns request
/// canonicalization; the Session application owns desired-state persistence,
/// idempotency, and realization ordering.
#[derive(Clone, Debug)]
pub struct ReplaceSessionResourceManifest {
    pub resources: ResolvedSessionResources,
    pub expected_session_revision: Option<SessionRevision>,
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
    resources: &awaken_session_contract::ResolvedSessionResources,
) -> Result<std::collections::BTreeSet<ResourceTarget>, ResourcePurgeError> {
    let mut targets = std::collections::BTreeSet::new();
    for input in &resources.inputs {
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
    if let Some(skills) = &resources.skills {
        targets.extend(
            skills
                .iter()
                .filter(|skill| skill.kind == awaken_agent_contract::AgentSkillKind::Custom)
                .map(|skill| ResourceTarget::new(workspace, ResourceKind::Skill, &skill.skill_id)),
        );
    }
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
            let candidates = resource_targets(
                self.files.as_ref(),
                &workspace,
                &session.resources.reference_manifest(),
            )
            .await?;
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
    async fn synchronize_resource_references(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let (Some(references), Some(files)) = (&self.resource_references, &self.resource_files)
        else {
            return Ok(());
        };
        let targets = resource_targets(
            files.as_ref(),
            owner_scope,
            &session.resources.reference_manifest(),
        )
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
        command.resources.validate().map_err(|error| {
            SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
        })?;
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
                    if command.expected_session_revision.is_none()
                        && attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
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
            .expected_session_revision
            .is_some_and(|expected| expected != session.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
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
        if outcome.session.resources.pending.is_none() {
            return Ok(outcome);
        }
        let owner_scope = self
            .owner(&outcome.session.session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let convergence = if self.requires_external_realization(&outcome.session) {
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
        token: awaken_agent_contract::RedactedString,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let ingress = self.repository_credential_ingress().ok_or_else(|| {
            SessionPreparationError::Rejected(RunError::bad_request(
                "repository authorization requires a configured credential Vault",
            ))
        })?;
        let mut persisted = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_preparation)?;
        let credential_source = persisted
            .resources
            .active
            .inputs
            .iter()
            .find(|input| input.binding_id == *binding_id)
            .and_then(|input| match &input.source {
                ResolvedInputSource::Repository { config, .. } => config.credential_binding.clone(),
                _ => None,
            })
            .ok_or_else(|| {
                SessionPreparationError::Rejected(RunError::bad_request(
                    "repository credential update requires an authenticated Repository resource",
                ))
            })?;
        ingress
            .rotate_repository_token(
                &awaken_credential_contract::CredentialSourceId(credential_source),
                owner_scope,
                token,
            )
            .await
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(format!(
                    "repository authorization could not be rotated: {error}"
                )))
            })?;

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let holder = self.resource_plaintext_holder(&persisted)?;
            let input = persisted
                .resources
                .active
                .inputs
                .iter_mut()
                .find(|input| input.binding_id == *binding_id)
                .ok_or(SessionPreparationError::NotFound)?;
            let ResolvedInputSource::Repository { credential, .. } = &mut input.source else {
                return Err(SessionPreparationError::NotFound);
            };
            *credential = None;
            self.pin_repository_credential(owner_scope, &holder, input)
                .await?;
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
        let mut report = SessionReconciliation::default();
        let sessions = match self.session_repository().reconcilable_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        report.pending = sessions.sessions.len();
        report.quarantined.clone_from(&sessions.quarantined);
        for scoped in sessions.sessions {
            let owner_scope = scoped.workspace_id;
            let session = scoped.session;
            let session_id = session.session_id.clone();
            if let Err(error) = self
                .synchronize_resource_references(&owner_scope, &session)
                .await
            {
                report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                });
                continue;
            }
            if (!session.is_terminal() && self.requires_external_realization(&session))
                || !session.needs_resource_reconciliation()
            {
                continue;
            }
            match self
                .reconcile_persisted_resources(&owner_scope, session)
                .await
            {
                Ok(session) => report.settled.push(session),
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
        mut session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        session = self
            .ensure_repository_credentials_pinned(owner_scope, session)
            .await?;
        if !session.is_terminal() && self.requires_external_realization(&session) {
            return Ok(session);
        }
        let session_id = session.session_id.clone();
        if session.execution != SessionExecutionState::Idle && !session.is_terminal() {
            return Ok(session);
        }
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
                return self
                    .settle_resource_reconciliation(
                        owner_scope,
                        &session_id,
                        session.resources.revision,
                        &desired,
                        ResourceSettlement::Commit,
                    )
                    .await;
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
            if session.resources.active.inputs.is_empty()
                && session.resources.activations.is_empty()
            {
                return Ok(session);
            }
            self.runtime()
                .apply_session_inputs(
                    &session_id,
                    owner_scope,
                    session.resources.revision,
                    &session.resources.active,
                )
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
            return Ok(session);
        }

        match self
            .release_terminal_resources(owner_scope, &session_id)
            .await?
        {
            Some(session) => Ok(session),
            None => Ok(session),
        }
    }

    /// The sole terminal cleanup implementation shared by archive/delete edges
    /// and background recovery. The target set comes only from the Runtime's
    /// durable delegation authority after the terminal fence has stopped the
    /// parent; protocol projections are never accepted as cleanup authority.
    /// Every frozen child Runtime is attempted even when another teardown fails;
    /// durable completion commits only when all external effects succeed.
    pub async fn release_terminal_resources(
        &self,
        owner_scope: &str,
        session_id: &str,
    ) -> Result<Option<PersistedSession>, SessionPreparationError> {
        let mut session = match self.session_repository().get(session_id).await {
            Ok(session) => session,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => return Ok(None),
            Err(error) => return Err(repository_preparation(error)),
        };
        if !session.terminal_cleanup.is_completed() {
            // Persist the admission fence before *any* external cleanup effect.
            // This also upgrades legacy terminal rows that predate the explicit
            // cleanup operation.
            if session.ensure_terminal_cleanup_fence() {
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "terminal-cleanup-fence",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
            }
            // Work retirement belongs to the recoverable cleanup operation.
            // Completed archive cleanup is absorbing: a replay must not issue a
            // second retirement attempt through this or any other reconciler.
            self.retire_terminal_work(&session).await?;

            // Phase 2 interrupts and waits for the parent to settle, then freezes the
            // complete durable delegated-Run set and its committed watermark. A retry
            // after this commit reuses exactly these targets.
            let mut intent_changed = false;
            if session.terminal_cleanup.is_fenced() {
                let snapshot = self
                    .runtime()
                    .quiesce_terminal_delegations(session_id)
                    .await
                    .map_err(SessionPreparationError::Rejected)?;
                intent_changed = session
                    .freeze_terminal_cleanup_targets(
                        snapshot
                            .delegated_runs
                            .into_iter()
                            .map(|delegated| delegated.run_id.0),
                        snapshot.watermark,
                    )
                    .map_err(internal)?;
            }
            if intent_changed {
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-release-intent",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
            }

            if session.terminal_cleanup.is_requested() {
                let threads = session
                    .terminal_cleanup
                    .thread_ids()
                    .cloned()
                    .ok_or_else(|| {
                        internal("Session terminal cleanup thread intent disappeared")
                    })?;
                let mut teardown_error = None;
                let mut receipts = Vec::with_capacity(threads.len());
                for thread in threads {
                    let command = session
                        .terminal_cleanup
                        .command_for(session_id, &thread)
                        .ok_or_else(|| internal("Session cleanup command disappeared"))?;
                    match self
                        .runtime()
                        .execute_terminal_cleanup(command.clone())
                        .await
                    {
                        Ok(completion) => match completion.verify(&command) {
                            Ok(receipt) => receipts.push(receipt),
                            Err(error) => {
                                teardown_error.get_or_insert(RunError::internal(format!(
                                    "Session cleanup completion mismatch: {error}"
                                )));
                            }
                        },
                        Err(error) => {
                            tracing::warn!(
                                session = session_id,
                                thread = %thread,
                                effect_id = %command.effect_id,
                                error = ?error,
                                "Session terminal Runtime teardown remains pending"
                            );
                            teardown_error.get_or_insert(error);
                        }
                    }
                }
                if let Some(error) = teardown_error {
                    return Err(SessionPreparationError::Rejected(error));
                }
                if let Some(checkpoint) = session.environment.checkpoint().cloned() {
                    self.runtime()
                        .delete_session_checkpoint(session_id, &checkpoint)
                        .await
                        .map_err(SessionPreparationError::Rejected)?;
                }
                if !self
                    .retire_session_repositories(owner_scope, session_id, &session.resources)
                    .await
                {
                    return Err(internal(
                        "Session-scoped Repository cleanup remains pending",
                    ));
                }
                session
                    .complete_terminal_cleanup(
                        &receipts,
                        "Session terminated before activation completed",
                    )
                    .map_err(internal)?;
            }
            session = self
                .commit_resource_snapshot(
                    owner_scope,
                    session,
                    "resource-release-complete",
                    Vec::new(),
                )
                .await
                .map_err(mutation_failure)?;
        }
        if session.is_hidden() {
            self.commit_delete_tombstone(owner_scope, &session)
                .await
                .map_err(mutation_failure)?;
            return Ok(None);
        }
        Ok(Some(session))
    }

    pub async fn retire_session_repositories(
        &self,
        owner_scope: &str,
        session_id: &str,
        resources: &awaken_session_contract::SessionResourceState,
    ) -> bool {
        if self.resource_catalog().is_none() {
            return true;
        }
        let prefix = format!("managed:{session_id}:repository:");
        let mut ids = std::collections::BTreeSet::new();
        for manifest in std::iter::once(&resources.active).chain(resources.pending.iter()) {
            for input in &manifest.inputs {
                if let ResolvedInputSource::Repository { repository_id, .. } = &input.source
                    && repository_id.as_str().starts_with(&prefix)
                {
                    ids.insert(repository_id.to_string());
                }
            }
        }
        let mut retired = true;
        for repository_id in ids {
            retired &= self.retire_repository(owner_scope, &repository_id).await;
        }
        retired
    }

    pub async fn retire_repository(&self, owner_scope: &str, repository_id: &str) -> bool {
        let Some(catalog) = self.resource_catalog() else {
            return true;
        };
        let definition = match catalog.repository(owner_scope, repository_id) {
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
            .set_repository_state(
                owner_scope,
                repository_id,
                awaken_resource_contract::ResourceState::Deleted,
            )
            .is_ok()
    }
}
