//! Live-session typed input CRUD and durable resource activation coordination.
//! The persisted [`SessionResourceState`](awaken_session_contract::SessionResourceState)
//! is authoritative; runtime mounts and wire DTOs are projections of its active
//! manifest.

use super::*;

#[derive(Clone)]
enum ResourceSettlement {
    Commit,
    Rollback(String),
    RetryableFailure(String),
}

impl ManagedState {
    fn resource_plaintext_holder(
        session: &PersistedSession,
    ) -> Result<awaken_credential_contract::PlaintextHolder, StateError> {
        session
            .frozen_baseline()
            .map(|baseline| {
                baseline
                    .environment
                    .credential_realization
                    .resource_holder
                    .clone()
            })
            .ok_or_else(|| {
                StateError::Run(RunError::bad_request(
                    "Session resource mutation requires a frozen Environment baseline",
                ))
            })
    }

    /// Compile or verify the exact Repository credential execution pin before
    /// the input enters the Session aggregate. This is the sole Resource→Vault
    /// selection seam; Runtime receives no bare credential binding.
    pub(super) async fn pin_repository_credential(
        &self,
        owner_scope: &str,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        input: &mut awaken_session_contract::ResolvedInput,
    ) -> Result<(), StateError> {
        let awaken_session_contract::ResolvedInputSource::Repository {
            config, credential, ..
        } = &mut input.source
        else {
            return Ok(());
        };
        let Some(binding) = config.credential_binding.as_deref() else {
            if credential.is_some() {
                return Err(StateError::Run(RunError::bad_request(
                    "Repository without a Vault binding carries a credential pin",
                )));
            }
            return Ok(());
        };
        if let Some(existing) = credential {
            existing
                .validate_for_binding(binding)
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            if &existing.selected_plaintext_holder != selected_holder {
                return Err(StateError::Run(RunError::bad_request(
                    "Repository credential pin selects another Environment holder",
                )));
            }
            return Ok(());
        }
        let vaults = self.vaults.as_ref().ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "Repository credential requires a configured credential vault",
            ))
        })?;
        let access = vaults
            .credential_access_for_source(
                &awaken_credential_vault::CredentialSourceId(binding.to_string()),
                owner_scope,
                awaken_session_contract::repository_transport_credential_usage(),
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    selected_holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
            )
            .await
            .map_err(|error| {
                StateError::Run(RunError::bad_request(format!(
                    "Repository credential could not be pinned exactly: {error}"
                )))
            })?;
        *credential = Some(Box::new(
            awaken_session_contract::ResolvedRepositoryCredential {
                access,
                selected_plaintext_holder: selected_holder.clone(),
            },
        ));
        Ok(())
    }

    /// Apply the one per-input compiler to a complete Resource generation.
    /// Creation, hot mutation, and retained-row migration share this traversal;
    /// callers never grow a second binding-to-access loop.
    pub(super) async fn pin_repository_credentials(
        &self,
        owner_scope: &str,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        resources: &mut awaken_session_contract::ResolvedSessionResources,
    ) -> Result<bool, StateError> {
        let before = resources.clone();
        for input in &mut resources.inputs {
            self.pin_repository_credential(owner_scope, selected_holder, input)
                .await?;
        }
        Ok(*resources != before)
    }

    /// One-time, root-CAS migration for retained Session rows written before the
    /// exact Repository pin existed. It runs before every realization entry and
    /// commits the secret-free pin before Runtime I/O. Existing pins are verified,
    /// never refreshed or reselected.
    pub(super) async fn ensure_repository_credentials_pinned(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, StateError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let holder = Self::resource_plaintext_holder(&session)?;
            let mut changed = self
                .pin_repository_credentials(owner_scope, &holder, &mut session.resources.active)
                .await?;
            if let Some(pending) = &mut session.resources.pending {
                changed |= self
                    .pin_repository_credentials(owner_scope, &holder, pending)
                    .await?;
            }
            if !changed {
                return Ok(session);
            }
            let session_id = session.session_id.clone();
            match self
                .commit_session_snapshot(
                    owner_scope,
                    session,
                    "repository-credential-pin-migration",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => return Ok(session),
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    session = self
                        .sessions_repo
                        .get(&session_id)
                        .await
                        .ok_or(StateError::NotFound)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(StateError::Conflict)
    }

    async fn prepare_resource_transition(
        &self,
        owner_scope: &str,
        mut current: PersistedSession,
        desired: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<PersistedSession, StateError> {
        let unchanged_resources = current.resources.clone();
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if current.resources != unchanged_resources {
                return Err(StateError::Conflict);
            }
            let mut candidate = current.clone();
            candidate
                .resources
                .prepare(&candidate.session_id, desired.clone())
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            candidate
                .resources
                .start_attempt()
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
            match self
                .commit_session_snapshot(owner_scope, candidate, "resource-prepare", Vec::new())
                .await
            {
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    current = self
                        .sessions_repo
                        .get(&current.session_id)
                        .await
                        .ok_or(StateError::NotFound)?;
                }
                result => return result,
            }
        }
        Err(StateError::Conflict)
    }

    async fn settle_resource_transition(
        &self,
        owner_scope: &str,
        session_id: &str,
        resource_revision: u64,
        desired: &awaken_session_contract::ResolvedSessionResources,
        settlement: ResourceSettlement,
    ) -> Result<PersistedSession, StateError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut current = self
                .sessions_repo
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            if current.resources.revision != resource_revision
                || current.resources.pending.as_ref() != Some(desired)
            {
                return Err(StateError::Conflict);
            }
            let operation = match &settlement {
                ResourceSettlement::Commit => {
                    current
                        .resources
                        .commit()
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                    "resource-activate"
                }
                ResourceSettlement::Rollback(error) => {
                    current
                        .resources
                        .rollback(error.clone())
                        .map_err(|state_error| {
                            StateError::Run(RunError::internal(state_error.to_string()))
                        })?;
                    "resource-rollback"
                }
                ResourceSettlement::RetryableFailure(error) => {
                    current
                        .resources
                        .note_retryable_failure(error.clone())
                        .map_err(|state_error| {
                            StateError::Run(RunError::internal(state_error.to_string()))
                        })?;
                    "resource-retryable-failure"
                }
            };
            match self
                .commit_session_snapshot(owner_scope, current, operation, Vec::new())
                .await
            {
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                result => return result,
            }
        }
        Err(StateError::Conflict)
    }

    async fn activate_inputs(
        &self,
        persisted: PersistedSession,
        owner_scope: &str,
        desired: awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), StateError> {
        let session_id = persisted.session_id.clone();
        let previous = persisted.resources.active.clone();
        // Prepared/Releasing is durable before the first external side effect.
        let persisted = self
            .prepare_resource_transition(owner_scope, persisted, &desired)
            .await?;
        let resource_revision = persisted.resources.revision;

        if let Err(error) = self
            .runtime
            .apply_session_inputs(&session_id, owner_scope, &desired)
            .await
        {
            // Restore the prior projection before reporting synchronous failure.
            // If rollback also fails, retain the pending transition for the
            // ResourceReclaimer instead of pretending either generation won.
            let settlement = match self
                .runtime
                .apply_session_inputs(&session_id, owner_scope, &previous)
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
            return Err(StateError::Run(error));
        }

        // Active/Released is the second durable edge. A crash before it leaves
        // Prepared/Releasing and is safe to retry idempotently.
        let persisted = self
            .settle_resource_transition(
                owner_scope,
                &session_id,
                resource_revision,
                &desired,
                ResourceSettlement::Commit,
            )
            .await?;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(&session_id).ok_or(StateError::NotFound)?;
        record.resource_state = persisted.resources;
        Ok(())
    }

    async fn resolve_live_input(
        &self,
        session_id: &str,
        owner_scope: &str,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding_id: String,
        parsed: &ParsedSessionInput,
    ) -> Result<awaken_session_contract::ResolvedInput, StateError> {
        let repository_id = if let ParsedInputTarget::Repository {
            remote_url,
            initial_branch,
        } = &parsed.target
        {
            let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
                StateError::Run(RunError::bad_request(
                    "repository resources require a configured Resource Catalog",
                ))
            })?;
            let repository_id = format!("managed:{session_id}:repository:{binding_id}");
            catalog
                .create_repository(
                    awaken_resource_contract::RepositoryDefinition {
                        id: repository_id.clone().into(),
                        workspace_id: owner_scope.to_string(),
                        name: "Live Session repository".into(),
                        description: "Managed compatibility Session input".into(),
                        metadata: Default::default(),
                        state: awaken_resource_contract::ResourceState::Active,
                        current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
                        timestamps: Default::default(),
                    },
                    awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: repository_id.clone().into(),
                        version: awaken_resource_contract::ConfigVersion::INITIAL,
                        remote_url: remote_url.clone(),
                        credential_binding: None,
                        initial_branch: initial_branch.clone(),
                        clone_policy: awaken_resource_contract::ClonePolicy::default(),
                    },
                )
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            Some(awaken_resource_contract::RepositoryId::from(repository_id))
        } else {
            None
        };
        let binding = input_binding(binding_id, parsed, repository_id);
        let attachment = awaken_session_contract::SessionInputAttachment {
            binding,
            replaces: None,
        };
        let catalog = self.resource_catalog.as_deref().ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "live resources require a configured Resource Catalog",
            ))
        })?;
        let mut input = awaken_session_contract::SessionInputResolver::resolve_inputs(
            owner_scope,
            Some(catalog),
            &[],
            &[attachment],
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?
        .inputs
        .pop()
        .ok_or_else(|| StateError::Run(RunError::internal("resolved input is empty")))?;
        self.pin_repository_credential(owner_scope, selected_holder, &mut input)
            .await?;
        Ok(input)
    }

    pub fn list_resources(&self, id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record
            .resource_state
            .active
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(id, input))
            .collect())
    }

    pub async fn create_resource(
        &self,
        id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let parsed = parse_session_input(&body).ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "resource must be a file or github_repository with its backing id",
            ))
        })?;
        if matches!(parsed.target, ParsedInputTarget::MemoryStore(_)) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let persisted = self
            .sessions_repo
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        let selected_holder = Self::resource_plaintext_holder(&persisted)?;
        let current = persisted.resources.active.clone();
        let mut suffix = current.inputs.len();
        let binding_id = loop {
            let candidate = format!("session:{id}:live:{suffix}");
            if current
                .inputs
                .iter()
                .all(|input| input.binding_id.as_str() != candidate)
            {
                break candidate;
            }
            suffix += 1;
        };
        current
            .validate_new_binding(
                &awaken_resource_contract::BindingId::from(binding_id.clone()),
                &parsed.mount_path,
            )
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        let input = self
            .resolve_live_input(id, &owner_scope, &selected_holder, binding_id, &parsed)
            .await?;
        let next = current
            .attach(input.clone())
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        if let Err(error) = self.activate_inputs(persisted, &owner_scope, next).await {
            if let awaken_session_contract::ResolvedInputSource::Repository {
                repository_id, ..
            } = &input.source
            {
                let _ = self
                    .retire_repository(&owner_scope, repository_id.as_str())
                    .await;
            }
            return Err(error);
        }
        Ok(resolved_resource_dto(id, &input))
    }

    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<serde_json::Value, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        record
            .resource_state
            .active
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .map(|input| resolved_resource_dto(id, input))
            .ok_or(StateError::NotFound)
    }

    pub async fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .sessions_repo
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        let selected_holder = Self::resource_plaintext_holder(&persisted)?;
        let (current, previous) = {
            let current = persisted.resources.active.clone();
            let previous = current
                .inputs
                .iter()
                .find(|input| input.binding_id == binding_id)
                .cloned()
                .ok_or(StateError::NotFound)?;
            (current, previous)
        };
        let mut replacement = previous.clone();
        if let Some(path) = patch.get("mount_path").and_then(serde_json::Value::as_str) {
            replacement.mount_path = path.to_string();
        }
        if let Some(value) = patch.get("instructions") {
            replacement.instructions = value.as_str().map(str::to_string);
        }
        // Validate all side-effect-free fields before sealing a new credential or
        // publishing a Repository config version.
        current
            .replace(replacement.clone())
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        if patch.get("authorization_token").is_some() {
            return Err(StateError::Run(RunError::bad_request(
                "raw_repository_credentials_unsupported",
            )));
        }
        self.pin_repository_credential(&owner_scope, &selected_holder, &mut replacement)
            .await?;
        let next = current
            .replace(replacement.clone())
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        self.activate_inputs(persisted, &owner_scope, next).await?;
        self.get_resource(id, resource_id)
    }

    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .sessions_repo
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        let (current, input) = {
            let current = persisted.resources.active.clone();
            let input = current
                .inputs
                .iter()
                .find(|input| input.binding_id == binding_id)
                .cloned()
                .ok_or(StateError::NotFound)?;
            (current, input)
        };
        if matches!(
            input.source,
            awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let (next, removed) = current
            .detach(&binding_id)
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        self.activate_inputs(persisted, &owner_scope, next).await?;
        if let awaken_session_contract::ResolvedInputSource::Repository { repository_id, .. } =
            removed.source
            && let Some(catalog) = &self.resource_catalog
        {
            catalog
                .set_repository_state(
                    &owner_scope,
                    repository_id.as_str(),
                    awaken_resource_contract::ResourceState::Deleted,
                )
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        }
        Ok(())
    }
}
