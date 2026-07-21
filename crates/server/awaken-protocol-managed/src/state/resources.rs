//! Live-session typed input CRUD. The persisted `EffectiveSessionInputs` aggregate
//! is authoritative; runtime mounts and wire DTOs are projections of that value.

use super::*;

impl ManagedState {
    fn refresh_resource_projection(record: &mut SessionRecord) {
        record.session.resources = record
            .effective_inputs
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(&record.session.id, input))
            .collect();
    }

    async fn persist_inputs(
        &self,
        session_id: &str,
        owner_scope: &str,
        inputs: awaken_session_contract::EffectiveSessionInputs,
    ) -> Result<(), StateError> {
        let mut persisted = self
            .sessions_repo
            .get(session_id)
            .await
            .ok_or(StateError::NotFound)?;
        persisted.effective_inputs = inputs.clone();
        self.sessions_repo.save_owned(owner_scope, persisted).await;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        record.effective_inputs = inputs;
        Self::refresh_resource_projection(record);
        Ok(())
    }

    async fn resolve_live_input(
        &self,
        session_id: &str,
        owner_scope: &str,
        binding_id: String,
        parsed: &ParsedSessionInput,
    ) -> Result<awaken_session_contract::ResolvedInput, StateError> {
        let repository_id = if let ParsedInputTarget::Repository {
            remote_url,
            authorization_token,
            initial_branch,
        } = &parsed.target
        {
            let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
                StateError::Run(RunError::bad_request(
                    "repository resources require a configured Resource Catalog",
                ))
            })?;
            let repository_id = format!("managed:{session_id}:repository:{binding_id}");
            let credential_binding = match authorization_token {
                Some(token) => {
                    let vaults = self.vaults.as_ref().ok_or_else(|| {
                        StateError::Run(RunError::bad_request(
                            "repository authorization requires a configured credential vault",
                        ))
                    })?;
                    Some(
                        vaults
                            .enter_session_bearer(owner_scope, token.clone())
                            .await
                            .map_err(|error| {
                                StateError::Run(RunError::bad_request(format!(
                                    "repository credential could not be stored: {error}"
                                )))
                            })?
                            .0,
                    )
                }
                None => None,
            };
            catalog
                .create_repository(
                    awaken_resource_contract::RepositoryDefinition {
                        id: repository_id.clone(),
                        workspace_id: owner_scope.to_string(),
                        name: "Live Session repository".into(),
                        description: "Managed compatibility Session input".into(),
                        metadata: Default::default(),
                        state: awaken_resource_contract::ResourceState::Active,
                        current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
                    },
                    awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: repository_id.clone(),
                        version: awaken_resource_contract::ConfigVersion::INITIAL,
                        remote_url: remote_url.clone(),
                        credential_binding,
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
        awaken_session_contract::SessionInputResolver::resolve_inputs(
            owner_scope,
            catalog,
            &[],
            &[attachment],
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?
        .inputs
        .pop()
        .ok_or_else(|| StateError::Run(RunError::internal("resolved input is empty")))
    }

    pub fn list_resources(&self, id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record.session.resources.clone())
    }

    pub async fn create_resource(
        &self,
        id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let _guard = self.resource_mutations.lock().await;
        let parsed = parse_session_input(&body).ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "resource must be a file or github_repository with its backing id",
            ))
        })?;
        if matches!(parsed.target, ParsedInputTarget::MemoryStore(_)) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let current = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .ok_or(StateError::NotFound)?
            .effective_inputs
            .clone();
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
            .resolve_live_input(id, &owner_scope, binding_id, &parsed)
            .await?;
        let next = current
            .attach(input.clone())
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        if let Err(error) = self
            .runtime
            .apply_session_inputs(id, &owner_scope, &next)
            .await
        {
            if let awaken_session_contract::ResolvedInputSource::Repository {
                repository_id, ..
            } = &input.source
                && let Some(catalog) = &self.resource_catalog
            {
                let _ = catalog.set_repository_state(
                    &owner_scope,
                    repository_id.as_str(),
                    awaken_resource_contract::ResourceState::Deleted,
                );
            }
            return Err(StateError::Run(error));
        }
        self.persist_inputs(id, &owner_scope, next).await?;
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .and_then(|record| record.session.resources.last().cloned())
            .ok_or(StateError::NotFound)
    }

    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<serde_json::Value, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        record
            .session
            .resources
            .iter()
            .find(|resource| resource["id"] == resource_id)
            .cloned()
            .ok_or(StateError::NotFound)
    }

    pub async fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let _guard = self.resource_mutations.lock().await;
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let (current, index) = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let index = record
                .session
                .resources
                .iter()
                .position(|resource| resource["id"] == resource_id)
                .ok_or(StateError::NotFound)?;
            (record.effective_inputs.clone(), index)
        };
        let previous = current.inputs[index].clone();
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
        if let Some(token) = patch
            .get("authorization_token")
            .and_then(serde_json::Value::as_str)
        {
            let awaken_session_contract::ResolvedInputSource::Repository {
                repository_id,
                config: pinned,
            } = &previous.source
            else {
                return Err(StateError::Run(RunError::bad_request(
                    "authorization_token is valid only for github_repository",
                )));
            };
            let vaults = self.vaults.as_ref().ok_or_else(|| {
                StateError::Run(RunError::bad_request(
                    "repository authorization requires a configured credential vault",
                ))
            })?;
            let credential = vaults
                .enter_session_bearer(&owner_scope, token.to_string())
                .await
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
                StateError::Run(RunError::bad_request("Resource Catalog is not configured"))
            })?;
            let definition = catalog
                .repository(&owner_scope, repository_id.as_str())
                .ok_or(StateError::NotFound)?;
            let mut next_config = pinned.clone();
            next_config.version = definition
                .current_config_version
                .checked_next()
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request("config version exhausted"))
                })?;
            next_config.credential_binding = Some(credential.0);
            catalog
                .publish_repository_config(
                    &owner_scope,
                    definition.current_config_version,
                    next_config.clone(),
                )
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            replacement.source = awaken_session_contract::ResolvedInputSource::Repository {
                repository_id: repository_id.clone(),
                config: next_config,
            };
        }
        let next = current
            .replace(replacement.clone())
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        self.runtime
            .apply_session_inputs(id, &owner_scope, &next)
            .await
            .map_err(StateError::Run)?;
        self.persist_inputs(id, &owner_scope, next).await?;
        self.get_resource(id, resource_id)
    }

    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        let _guard = self.resource_mutations.lock().await;
        let owner_scope = self.resolve_owner(id).await.ok_or(StateError::NotFound)?;
        let (current, index) = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let index = record
                .session
                .resources
                .iter()
                .position(|resource| resource["id"] == resource_id)
                .ok_or(StateError::NotFound)?;
            (record.effective_inputs.clone(), index)
        };
        let input = &current.inputs[index];
        if matches!(
            input.source,
            awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let (next, removed) = current
            .detach(&input.binding_id)
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        self.runtime
            .apply_session_inputs(id, &owner_scope, &next)
            .await
            .map_err(StateError::Run)?;
        self.persist_inputs(id, &owner_scope, next).await?;
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
