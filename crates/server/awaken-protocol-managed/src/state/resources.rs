//! Live-session typed input CRUD and durable resource activation coordination.
//! The persisted [`SessionResourceState`](awaken_session_contract::SessionResourceState)
//! is authoritative; runtime mounts and wire DTOs are projections of its active
//! manifest.

use super::*;

impl ManagedState {
    /// ResourceReclaimer entry point. Composition roots call this after durable
    /// stores and the Runtime Host are wired. It scans only Session application
    /// state; authorization principals and policy objects never cross this seam.
    pub async fn reconcile_resource_activations(&self) -> usize {
        let report = self.application.reconcile_resource_activations().await;
        for session in &report.settled {
            if let Err(error) = self.refresh_cached_projection(session) {
                tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session Resource wire projection refresh remains pending"
                );
            }
        }
        for failure in report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session resource reconciliation remains pending"
            );
        }
        report.settled.len()
    }

    pub(super) fn map_preparation_error(
        error: awaken_session_application::SessionPreparationError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionPreparationError::NotFound => StateError::NotFound,
            awaken_session_application::SessionPreparationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionPreparationError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionPreparationError::Unavailable(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    fn resolve_live_file_input(
        &self,
        owner_scope: &str,
        binding_id: String,
        parsed: &ParsedSessionInput,
    ) -> Result<awaken_session_contract::ResolvedInput, StateError> {
        debug_assert!(matches!(parsed.target, ParsedInputTarget::File(_)));
        let binding = input_binding(binding_id, parsed, None);
        let attachment = awaken_session_contract::SessionInputAttachment {
            binding,
            replaces: None,
        };
        awaken_session_contract::SessionInputResolver::resolve_inputs(
            owner_scope,
            None,
            &[],
            &[attachment],
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?
        .inputs
        .pop()
        .ok_or_else(|| StateError::Run(RunError::internal("resolved input is empty")))
    }

    async fn lower_complete_resource_manifest(
        &self,
        session_id: &str,
        owner_scope: &str,
        resources: &[crate::types::resource::ResourceInput],
        current: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<awaken_session_contract::ResolvedSessionResources, StateError> {
        let mut parsed = resources
            .iter()
            .map(crate::types::resource::ResourceInput::to_parsed_input)
            .collect::<Vec<_>>();
        if parsed
            .iter()
            .filter(|input| matches!(input.target, ParsedInputTarget::File(_)))
            .count()
            > super::resource::MAX_SESSION_FILE_RESOURCES
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {} files",
                super::resource::MAX_SESSION_FILE_RESOURCES
            ))));
        }

        // Derive all paths and validate the complete set before Repository/Vault
        // side effects. This is the atomic command's fail-fast boundary.
        let mut used_mounts = parsed
            .iter()
            .filter(|resource| !resource.implicit_memory_mount)
            .map(|resource| resource.mount_path.trim_start_matches('/').to_string())
            .collect::<std::collections::BTreeSet<_>>();
        for resource in &mut parsed {
            if resource.implicit_memory_mount {
                let ParsedInputTarget::MemoryStore(memory_store_id) = &resource.target else {
                    unreachable!("only MemoryStore inputs derive a Managed mount path")
                };
                let definition = self
                    .application
                    .session_memory_store(owner_scope, memory_store_id.as_str())
                    .map_err(StateError::Run)?;
                resource.mount_path = super::resource::unique_memory_mount_path(
                    &definition.name,
                    memory_store_id.as_str(),
                    &used_mounts,
                );
                used_mounts.insert(resource.mount_path.trim_start_matches('/').to_string());
            }
        }
        let provisional = parsed
            .iter()
            .enumerate()
            .map(|(index, input)| {
                input_binding(
                    format!("manifest-validation-{index}"),
                    input,
                    matches!(input.target, ParsedInputTarget::Repository { .. })
                        .then(|| awaken_resource_contract::RepositoryId::from("validation")),
                )
            })
            .collect::<Vec<_>>();
        awaken_session_contract::SessionInputResolver::compose(&provisional, &[])
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;

        let mut inputs = Vec::with_capacity(parsed.len());
        for input in parsed {
            let normalized_mount = input.mount_path.trim_start_matches('/');
            let same_mount = current
                .inputs
                .iter()
                .find(|existing| existing.mount_path.trim_start_matches('/') == normalized_mount);
            let reusable = same_mount.filter(|existing| match (&input.target, &existing.source) {
                (
                    ParsedInputTarget::File(requested),
                    awaken_session_contract::ResolvedInputSource::File { file_id },
                ) => requested == file_id,
                (
                    ParsedInputTarget::MemoryStore(requested),
                    awaken_session_contract::ResolvedInputSource::MemoryStore {
                        memory_store_id,
                        ..
                    },
                ) => requested == memory_store_id,
                (
                    ParsedInputTarget::Repository {
                        remote_url,
                        authorization_token,
                        initial_branch,
                        initial_commit,
                    },
                    awaken_session_contract::ResolvedInputSource::Repository { config, .. },
                ) => {
                    authorization_token.is_none()
                        && remote_url == &config.remote_url
                        && initial_branch == &config.initial_branch
                        && initial_commit == &config.initial_commit
                }
                _ => false,
            });
            if let Some(existing) = reusable {
                let mut existing = existing.clone();
                existing.access = input.access;
                existing.instructions = input.instructions;
                inputs.push(existing);
                continue;
            }

            let binding_id = same_mount
                .map(|existing| existing.binding_id.to_string())
                .unwrap_or_else(|| {
                    format!(
                        "session:{session_id}:manifest:{}",
                        awaken_session_contract::stable_fingerprint(&normalized_mount)
                    )
                });
            let repository_id = if let ParsedInputTarget::Repository {
                remote_url,
                authorization_token,
                initial_branch,
                initial_commit,
            } = &input.target
            {
                let id = format!(
                    "managed:{session_id}:repository:{}",
                    awaken_session_contract::stable_fingerprint(&(
                        normalized_mount,
                        remote_url,
                        initial_branch,
                        initial_commit,
                    ))
                );
                Some(
                    self.application
                        .configure_session_repository(
                            awaken_session_application::SessionRepositoryResourceInput {
                                id,
                                workspace_id: owner_scope.to_string(),
                                name: format!("Session repository at /{normalized_mount}"),
                                description: "Managed Session resource manifest input".into(),
                                remote_url: remote_url.clone(),
                                authorization_token: authorization_token
                                    .clone()
                                    .map(|token| token.into_redacted()),
                                initial_branch: initial_branch.clone(),
                                initial_commit: initial_commit.clone(),
                            },
                        )
                        .await
                        .map_err(StateError::Run)?,
                )
            } else {
                None
            };
            let attachment = awaken_session_contract::SessionInputAttachment {
                binding: input_binding(binding_id, &input, repository_id),
                replaces: None,
            };
            let mut resolved = self
                .application
                .resolve_session_inputs(owner_scope, &[], &[attachment])
                .map_err(StateError::Run)?
                .inputs;
            inputs.push(resolved.pop().ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "resolved resource manifest input is empty",
                ))
            })?);
        }
        let manifest = awaken_session_contract::ResolvedSessionResources {
            inputs,
            skills: current.skills.clone(),
        };
        manifest
            .validate()
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        Ok(manifest)
    }

    pub async fn replace_resource_manifest(
        &self,
        id: &str,
        body: crate::types::resource::ResourceManifestReplaceParams,
        idempotency_key: Option<String>,
        expected_session_revision: Option<awaken_session_contract::SessionRevision>,
        request_fingerprint: String,
    ) -> Result<
        (
            crate::types::resource::SessionResourceManifest,
            awaken_session_contract::SessionRevision,
        ),
        StateError,
    > {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        if let Some(key) = idempotency_key.as_deref()
            && let Some(outcome) = self
                .application
                .replay_session_resource_manifest(id, key, &request_fingerprint)
                .await
                .map_err(Self::map_resource_manifest_error)?
        {
            self.refresh_cached_projection(&outcome.session)?;
            return Ok((
                Self::resource_manifest_view(&outcome.session),
                outcome.command_revision,
            ));
        }
        let persisted = self
            .application
            .session(id)
            .await
            .map_err(StateError::from)?;
        if expected_session_revision.is_some_and(|expected| expected != persisted.revision) {
            return Err(StateError::Conflict);
        }
        let desired = self
            .lower_complete_resource_manifest(
                id,
                &owner_scope,
                &body.resources,
                persisted.resources.desired(),
            )
            .await?;
        let outcome = self
            .application
            .replace_session_resource_manifest(
                id,
                awaken_session_application::ReplaceSessionResourceManifest {
                    resources: desired,
                    expected_session_revision,
                    idempotency_key,
                    request_fingerprint,
                },
            )
            .await
            .map_err(Self::map_resource_manifest_error)?;
        self.refresh_cached_projection(&outcome.session)?;
        Ok((
            Self::resource_manifest_view(&outcome.session),
            outcome.command_revision,
        ))
    }

    fn resource_manifest_view(
        session: &awaken_session_contract::PersistedSession,
    ) -> crate::types::resource::SessionResourceManifest {
        let applying = session.resources.pending.is_some();
        crate::types::resource::SessionResourceManifest {
            desired_revision: session.resources.revision,
            applied_revision: session.resources.active_revision(),
            phase: if applying {
                crate::types::resource::ResourceManifestPhase::Applying
            } else {
                crate::types::resource::ResourceManifestPhase::Active
            },
            resources: session
                .resources
                .desired()
                .inputs
                .iter()
                .map(|input| resolved_resource_dto(&session.session_id, input))
                .collect(),
        }
    }

    fn map_resource_manifest_error(
        error: awaken_session_application::SessionResourceManifestError,
    ) -> StateError {
        use awaken_session_application::SessionResourceManifestError as Error;
        match error {
            Error::NotFound => StateError::NotFound,
            Error::Terminal | Error::Conflict => StateError::Conflict,
            Error::IdempotencyMismatch => StateError::IdempotencyMismatch,
            Error::Rejected(error) => StateError::Run(error),
            Error::ProjectionAfterCommit { source, .. } => Self::map_preparation_error(source),
            Error::Unavailable(message) => StateError::Run(RunError::unavailable(message)),
        }
    }

    pub fn list_resources(
        &self,
        id: &str,
    ) -> Result<Vec<crate::types::resource::SessionResource>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record
            .resource_state
            .desired()
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(id, input))
            .collect())
    }

    pub async fn create_resource(
        &self,
        id: &str,
        body: crate::types::resource::ResourceAddParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let parsed = body.into_resource_input().to_parsed_input();
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .session(id)
            .await
            .map_err(StateError::from)?;
        let current = persisted.resources.desired().clone();
        if matches!(parsed.target, ParsedInputTarget::File(_))
            && current
                .inputs
                .iter()
                .filter(|input| {
                    matches!(
                        input.source,
                        awaken_session_contract::ResolvedInputSource::File { .. }
                    )
                })
                .count()
                >= super::resource::MAX_SESSION_FILE_RESOURCES
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {} files",
                super::resource::MAX_SESSION_FILE_RESOURCES
            ))));
        }
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
        let input = self.resolve_live_file_input(&owner_scope, binding_id, &parsed)?;
        let committed = self
            .application
            .attach_session_input(id, &owner_scope, input.clone())
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&committed)?;
        Ok(resolved_resource_dto(id, &input))
    }

    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        record
            .resource_state
            .desired()
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
        patch: crate::types::resource::ResourceUpdateParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .rotate_repository_credential(
                id,
                &owner_scope,
                &binding_id,
                patch.authorization_token.into_redacted(),
            )
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&persisted)?;
        let input = persisted
            .resources
            .desired()
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .ok_or(StateError::NotFound)?;
        Ok(resolved_resource_dto(id, input))
    }

    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .session(id)
            .await
            .map_err(StateError::from)?;
        let input = persisted
            .resources
            .desired()
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .cloned()
            .ok_or(StateError::NotFound)?;
        if matches!(
            input.source,
            awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let committed = self
            .application
            .detach_session_input(id, &owner_scope, &binding_id)
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&committed)?;
        Ok(())
    }
}
