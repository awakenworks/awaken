//! Durable Session Resource recovery and terminal reclamation.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceKind, ResourcePurgeError, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_session_contract::{
    ActivationState, ManagedLifecycleFact, PersistedSession, ResolvedInputSource, RunError,
    SessionLifecycleState, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRevision, SessionTombstone,
};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure,
};

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
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
            .try_reconcilable_sessions()
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        for scoped in sessions {
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

pub(crate) fn internal(error: impl std::fmt::Display) -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::internal(error.to_string()))
}

fn deleted_lifecycle_fact(session_id: &str, owner_scope: &str) -> ManagedLifecycleFact {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    ManagedLifecycleFact {
        id: format!("session:{session_id}:deleted"),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.deleted".into(),
        timestamp,
    }
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

    pub(crate) async fn repair_resource_references(&self, owner_scope: &str, session_id: &str) {
        let authoritative = self.session_repository().get(session_id).await;
        let repair = match authoritative {
            Some(session) => {
                self.synchronize_resource_references(owner_scope, &session)
                    .await
            }
            None => match &self.resource_references {
                Some(references) => references
                    .replace_references(
                        ResourceReferenceKind::SessionBinding,
                        session_id,
                        Vec::new(),
                    )
                    .await
                    .map_err(internal),
                None => Ok(()),
            },
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

    /// Attach one already-resolved neutral input and converge the Runtime before
    /// returning the committed aggregate.
    pub async fn attach_session_input(
        &self,
        session_id: &str,
        owner_scope: &str,
        input: awaken_session_contract::ResolvedInput,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let persisted = self
            .session_repository()
            .get(session_id)
            .await
            .ok_or(SessionPreparationError::NotFound)?;
        let desired = persisted
            .resources
            .desired()
            .attach(input)
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
            })?;
        if persisted.resources.pending.is_some() {
            return self
                .revise_pending_resource_transition(owner_scope, persisted, desired)
                .await;
        }
        self.activate_session_inputs(persisted, owner_scope, desired)
            .await
    }

    /// Detach one neutral binding and converge the Runtime. Repository
    /// definitions are retired only after the durable/Runtime replacement wins.
    pub async fn detach_session_input(
        &self,
        session_id: &str,
        owner_scope: &str,
        binding_id: &awaken_resource_contract::BindingId,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let persisted = self
            .session_repository()
            .get(session_id)
            .await
            .ok_or(SessionPreparationError::NotFound)?;
        let (desired, removed) =
            persisted
                .resources
                .desired()
                .detach(binding_id)
                .map_err(|error| {
                    SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
        let committed = if persisted.resources.pending.is_some() {
            self.revise_pending_resource_transition(owner_scope, persisted, desired)
                .await?
        } else {
            self.activate_session_inputs(persisted, owner_scope, desired)
                .await?
        };
        if let ResolvedInputSource::Repository { repository_id, .. } = removed.source
            && !self
                .retire_repository(owner_scope, repository_id.as_str())
                .await
        {
            return Err(internal(format!(
                "Repository `{repository_id}` retirement remains pending"
            )));
        }
        Ok(committed)
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
            .ok_or(SessionPreparationError::NotFound)?;
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
                        .ok_or(SessionPreparationError::NotFound)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    async fn prepare_resource_transition(
        &self,
        owner_scope: &str,
        mut current: PersistedSession,
        desired: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let unchanged_resources = current.resources.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if current.resources != unchanged_resources {
                return Err(SessionPreparationError::Conflict);
            }
            let mut candidate = current.clone();
            candidate
                .resources
                .prepare(&candidate.session_id, desired.clone())
                .map_err(|error| {
                    SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
            candidate.resources.start_attempt().map_err(internal)?;
            match self
                .commit_resource_snapshot(owner_scope, candidate, "resource-prepare", Vec::new())
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    current = self
                        .session_repository()
                        .get(&current.session_id)
                        .await
                        .ok_or(SessionPreparationError::NotFound)?;
                }
                result => return result.map_err(mutation_failure),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    async fn revise_pending_resource_transition(
        &self,
        owner_scope: &str,
        mut current: PersistedSession,
        desired: awaken_session_contract::ResolvedSessionResources,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let unchanged_resources = current.resources.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if current.resources != unchanged_resources {
                return Err(SessionPreparationError::Conflict);
            }
            let mut candidate = current.clone();
            candidate
                .resources
                .revise_unattempted_pending(&candidate.session_id, desired.clone())
                .map_err(|error| {
                    SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
            match self
                .commit_resource_snapshot(
                    owner_scope,
                    candidate,
                    "resource-revise-pending",
                    Vec::new(),
                )
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    current = self
                        .session_repository()
                        .get(&current.session_id)
                        .await
                        .ok_or(SessionPreparationError::NotFound)?;
                }
                result => return result.map_err(mutation_failure),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    async fn settle_resource_transition(
        &self,
        owner_scope: &str,
        session_id: &str,
        resource_revision: u64,
        desired: &awaken_session_contract::ResolvedSessionResources,
        settlement: ResourceSettlement,
    ) -> Result<PersistedSession, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut current = self
                .session_repository()
                .get(session_id)
                .await
                .ok_or(SessionPreparationError::NotFound)?;
            if current.resources.revision != resource_revision
                || current.resources.pending.as_ref() != Some(desired)
            {
                return Err(SessionPreparationError::Conflict);
            }
            let operation = match &settlement {
                ResourceSettlement::Commit => {
                    current.resources.commit().map_err(internal)?;
                    "resource-activate"
                }
                ResourceSettlement::Rollback(error) => {
                    current
                        .resources
                        .rollback(error.clone())
                        .map_err(internal)?;
                    "resource-rollback"
                }
                ResourceSettlement::RetryableFailure(error) => {
                    current
                        .resources
                        .note_retryable_failure(error.clone())
                        .map_err(internal)?;
                    "resource-retryable-failure"
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

    /// Apply one live Resource replacement through the same durable phase
    /// protocol consumed by recovery.
    pub async fn activate_session_inputs(
        &self,
        persisted: PersistedSession,
        owner_scope: &str,
        desired: awaken_session_contract::ResolvedSessionResources,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let session_id = persisted.session_id.clone();
        let previous = persisted.resources.active.clone();
        let persisted = self
            .prepare_resource_transition(owner_scope, persisted, &desired)
            .await?;
        let resource_revision = persisted.resources.revision;
        if let Err(error) = self
            .runtime()
            .apply_session_inputs(&session_id, owner_scope, resource_revision, &desired)
            .await
        {
            let settlement = match self
                .runtime()
                .apply_session_inputs(
                    &session_id,
                    owner_scope,
                    resource_revision.saturating_sub(1),
                    &previous,
                )
                .await
            {
                Ok(()) => ResourceSettlement::Rollback(error.to_string()),
                Err(rollback_error) => ResourceSettlement::RetryableFailure(format!(
                    "activation failed: {error}; rollback failed: {rollback_error}"
                )),
            };
            self.settle_resource_transition(
                owner_scope,
                &session_id,
                resource_revision,
                &desired,
                settlement,
            )
            .await?;
            return Err(SessionPreparationError::Rejected(error));
        }
        self.settle_resource_transition(
            owner_scope,
            &session_id,
            resource_revision,
            &desired,
            ResourceSettlement::Commit,
        )
        .await
    }

    /// Commit the terminal delete tombstone through the canonical Session CAS.
    pub async fn tombstone_session_snapshot(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionMutationError> {
        let deleted_revision =
            SessionRevision(session.revision.0.checked_add(1).ok_or_else(|| {
                SessionMutationError::Unavailable("Session revision exhausted".into())
            })?);
        let payload = SessionMutationPayload::Delete(SessionTombstone {
            session_id: session.session_id.clone(),
            deleted_revision,
            deleted_at: fact.timestamp.to_string(),
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
            lifecycle_facts: vec![fact],
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
        let sessions = match self.session_repository().try_reconcilable_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        for scoped in sessions {
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
        if session.lifecycle != SessionLifecycleState::Idle && !session.is_terminal() {
            return Ok(session);
        }
        if session.lifecycle == SessionLifecycleState::Idle {
            if let Some(desired) = session.resources.pending.clone() {
                session.resources.start_attempt().map_err(internal)?;
                session = self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-attempt",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
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
                    session
                        .resources
                        .note_retryable_failure(error.to_string())
                        .map_err(internal)?;
                    self.commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-failed",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
                    return Err(SessionPreparationError::Rejected(error));
                }
                session.resources.commit().map_err(internal)?;
                return self
                    .commit_resource_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-active",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure);
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
            .release_terminal_resources(owner_scope, &session_id, &[])
            .await?
        {
            Some(session) => Ok(session),
            None => Ok(session),
        }
    }

    /// The sole terminal cleanup implementation shared by archive/delete edges
    /// and background recovery. Every supplied child Runtime is attempted even
    /// when another teardown fails; durable release completion commits only when
    /// all external effects succeed.
    pub async fn release_terminal_resources(
        &self,
        owner_scope: &str,
        session_id: &str,
        child_thread_ids: &[String],
    ) -> Result<Option<PersistedSession>, SessionPreparationError> {
        let Some(mut session) = self.session_repository().get(session_id).await else {
            return Ok(None);
        };
        if session.resources.pending.is_none() {
            session.resources.begin_release().map_err(internal)?;
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

        let mut threads = std::collections::BTreeSet::from([session_id.to_string()]);
        threads.extend(child_thread_ids.iter().cloned());
        let mut teardown_error = None;
        for thread in threads {
            if let Err(error) = self.runtime().end_session(&thread).await {
                tracing::warn!(
                    session = session_id,
                    thread = %thread,
                    error = ?error,
                    "Session terminal Runtime teardown remains pending"
                );
                teardown_error.get_or_insert(error);
            }
        }
        if let Some(error) = teardown_error {
            return Err(SessionPreparationError::Rejected(error));
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
            .resources
            .complete_terminal_release("Session terminated before activation completed");
        session = self
            .commit_resource_snapshot(
                owner_scope,
                session,
                "resource-release-complete",
                Vec::new(),
            )
            .await
            .map_err(mutation_failure)?;
        if session.lifecycle == SessionLifecycleState::Deleted {
            self.tombstone_session_snapshot(
                owner_scope,
                &session,
                deleted_lifecycle_fact(session_id, owner_scope),
            )
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
