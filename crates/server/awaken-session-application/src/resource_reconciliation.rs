//! Durable Session Resource recovery and terminal reclamation.

use awaken_session_contract::{
    ActivationState, ManagedLifecycleFact, PersistedSession, ResolvedInputSource, RunError,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRevision,
    SessionTombstone,
};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError, SessionReconciliation,
    SessionReconciliationFailure,
};

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
        for scoped in self.session_repository().reconcilable_sessions().await {
            let owner_scope = scoped.workspace_id;
            let session = scoped.session;
            if self.requires_external_realization(&session)
                || !session.needs_resource_reconciliation()
            {
                continue;
            }
            let session_id = session.session_id.clone();
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
        if self.requires_external_realization(&session) {
            return Ok(session);
        }
        let session_id = session.session_id.clone();
        if session.status != "idle" && !session.is_terminal() {
            return Ok(session);
        }
        if session.status == "idle" {
            if let Some(desired) = session.resources.pending.clone() {
                session.resources.start_attempt().map_err(internal)?;
                session = self
                    .commit_session_snapshot(
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
                    self.commit_session_snapshot(
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
                    .commit_session_snapshot(
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
                    .commit_session_snapshot(
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
                .commit_session_snapshot(
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
            .commit_session_snapshot(
                owner_scope,
                session,
                "resource-release-complete",
                Vec::new(),
            )
            .await
            .map_err(mutation_failure)?;
        if session.status == "deleted" {
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
