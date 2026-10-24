// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
// @formal entrypoint skill.invalid_workspace_or_hash_fails_closed
// @formal entrypoint skill.loaded_version_equals_session_pin
//! Requirement compilation of Resource staging: the frozen Resource inputs a
//! transition stages (Files, Memory, Skills, Repositories), the live Memory
//! binding verification they require, and the exact frozen Skill bundle reads.
use super::ResourceStaging;
use awaken_session_configuration_contract::environment_projection;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;

use awaken_session_types::RunError;

use crate::managed_input_layout::{
    ManagedLayoutProjection, ResourcePromptProjection, managed_mount_access,
    resolved_input_layout_projection,
};
use crate::skill_delivery::skill_store_run_error;
use crate::staged_resources::CompiledEffectiveInputs;

use super::projection::repository_binding_run_error;
use crate::skill_delivery::anthropic_skill;
use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{SkillStoreError, SkillVersion};
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;

impl ResourceStaging {
    /// Resolve the exact Skill bytes selected by one aggregate-authored
    /// Resource generation. Both dispatch-only semantic recovery and complete
    /// physical requirement compilation enter here, so a cold replica cannot
    /// invent a second catalog or hash-validation path.
    pub async fn load_pinned(
        &self,
        workspace: &str,
        bindings: &[ResolvedSkillBinding],
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(&str, u64, &session_resource::SessionRealizationLease)>,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        let realization = if self.fetches_resources_upstream {
            realization
        } else {
            None
        };

        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let version = match binding.kind {
                AgentSkillKind::Anthropic => anthropic_skill(&binding.skill_id)
                    .filter(|version| version.version == binding.version)
                    .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?,
                AgentSkillKind::Custom => {
                    let authority = match (claim, realization) {
                        (_, Some((session_id, revision, lease))) => Some(
                            ResourceOperationFence::SessionMaterialization(
                                awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
                                    session_id,
                                    revision,
                                    binding.clone(),
                                    lease.clone(),
                                )
                                .map_err(|error| SkillStoreError::InvalidSelection {
                                    detail: error,
                                })?,
                            ),
                        ),
                        (Some(claim), None) => {
                            Some(ResourceOperationFence::Run(claim.clone()))
                        }
                        (None, None) => None,
                    };
                    self.bundle_source
                        .as_ref()
                        .ok_or_else(|| SkillStoreError::AuthorityUnavailable {
                            detail: "no Skill bundle source".into(),
                        })?
                        .load(workspace, binding, authority.as_ref())
                        .await
                        .map_err(|error| SkillStoreError::Indeterminate {
                            detail: error.to_string(),
                        })?
                        .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?
                }
            };
            if version.bundle_sha256 != binding.bundle_sha256 {
                return Err(SkillStoreError::CorruptData {
                    detail: format!(
                        "Skill {} version {} hash changed",
                        binding.skill_id, binding.version
                    ),
                });
            }
            versions.push(version);
        }
        Ok(versions)
    }

    pub async fn retained_resource_requirements(
        &self,
        thread: &str,
        desired: &session_resource::SessionResourceManifest,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
        effect_key: &str,
    ) -> Result<CompiledEffectiveInputs, RunError> {
        let sessions = &self.sessions;
        let cache_matches = sessions
            .read_realization(thread, |r| {
                r.staging().effect_key.as_deref() == Some(effect_key)
            })?
            .unwrap_or(false);
        if !cache_matches {
            // Cache misses converge on the same cold-requirements compiler used
            // by complete projection staging. This is the only Skill/Resource
            // read and requirements-publication owner; the physical body below
            // consumes its result and publishes only completion.
            self.stage_resource_requirements(
                thread,
                desired,
                claim,
                realization_lease.map(|lease| (thread, desired.revision, lease)),
                effect_key.to_owned(),
            )
            .await?;
        }
        sessions
            .read_realization(thread, |r| {
                (r.staging().effect_key.as_deref() == Some(effect_key)).then(|| {
                    CompiledEffectiveInputs {
                        staged: r.staging().resources.clone(),
                        memory_bindings: r.staging().memory_bindings.clone(),
                    }
                })
            })?
            .flatten()
            .ok_or_else(|| {
                RunError::internal(
                    "Session Resource requirements were not retained under the exact transition",
                )
            })
    }

    pub async fn stage_resource_requirements(
        &self,
        thread: &str,
        desired: &session_resource::SessionResourceManifest,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(&str, u64, &session_resource::SessionRealizationLease)>,
        effect_key: String,
    ) -> Result<(), RunError> {
        let realizations = &self.sessions;
        // Unclaimed replay may reuse the exact already-compiled cold
        // requirements. Claimed staging intentionally revalidates immutable
        // bytes/configuration on every ingress operation before refreshing the
        // same cache entry.
        if claim.is_none()
            && realizations
                .read_realization(thread, |r| {
                    r.staging().effect_key.as_deref() == Some(effect_key.as_str())
                })?
                .unwrap_or(false)
        {
            return Ok(());
        }
        let workspace = desired.workspace_id.as_str();
        let resources = &desired.resources;
        let versions = self
            .load_pinned(workspace, resources.skills(), claim, realization)
            .await
            .map_err(skill_store_run_error)?;
        // Realize an already-resolved, secret-free manifest. The pinned Memory/
        // Repository configuration in `inputs` remains authoritative; the per-item
        // validation in `stage_resolved_input` checks only current ownership/state
        // and the frozen version's integrity. No Agent binding or current config is
        // configured here.

        let mut all = crate::staged_resources::StagedResources::default();
        let mut memory_bindings = std::collections::HashMap::new();
        let environment = realizations.frozen(thread)?.environment_projection.clone();
        for input in resources.inputs() {
            let one = self
                .stage_resolved_input(workspace, input, claim, realization, environment.as_ref())
                .await?;
            // Read this exact projection before merging it. Two bindings may
            // legally reference the same store with different access, and a
            // prior mount must never become the authority for the later one.
            let materialization_reference = one.mounts.iter().find_map(|mount| {
                if let awaken_provisioning_contract::MountSource::MemoryStore {
                    materialization_reference,
                    ..
                } = &mount.source
                {
                    materialization_reference.clone()
                } else {
                    None
                }
            });
            if let Some((binding_id, memory)) = self
                .memory_binding_compiler()
                .compile(thread, workspace, input, materialization_reference)
                .await?
            {
                memory_bindings.insert(binding_id, memory);
            }
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.resource_prompts.extend(one.resource_prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
        }

        let compiled = CompiledEffectiveInputs {
            staged: all,
            memory_bindings,
        };

        // Publish only after every fallible Resource/Skill read has succeeded.
        // The installed Session then exposes one complete set of cold requirements; failed
        // compilation leaves no Workspace, prompt, binding, or active-manifest
        // residue. Only the exact physical transition publishes the manifest.
        realizations.publish_resource_requirements(
            thread,
            workspace,
            realization.map(|(_, _, lease)| lease),
            compiled,
            versions,
            effect_key,
        )?;
        Ok(())
    }

    /// Compile one frozen Managed input into the provisioning vocabulary. This
    /// projection owns protocol paths and secret-free credential pins; the
    /// Repository effect edge owns the one typed material translation. Sandbox
    /// adapters consume only the resulting neutral requirements.
    pub async fn stage_resolved_input(
        &self,
        workspace: &str,
        input: &session_resource::ResolvedInput,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(&str, u64, &session_resource::SessionRealizationLease)>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_types::RunError> {
        let authority = if let (Some((session_id, resource_revision, lease)), true) =
            (realization, self.fetches_resources_upstream)
        {
            Some(ResourceOperationFence::SessionMaterialization(
                awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
                    session_id,
                    resource_revision,
                    input.clone(),
                    lease.clone(),
                )
                .map_err(awaken_session_types::RunError::bad_request)?,
            ))
        } else {
            claim.map(|claim| ResourceOperationFence::Run(claim.clone()))
        };
        self.stage_resolved_input_with_resource_authority(
            workspace,
            input,
            authority.as_ref(),
            realization,
            environment,
        )
        .await
    }
    /// Compile the exact Repository input carried by a terminal publication
    /// command through the same path as ordinary Session staging. A remote lease
    /// selects the HTTP authority verifier; a local command needs no mutable
    /// catalog re-read. Path, access, credential pin, transport, and
    /// realization-plan projection stay canonical here.
    pub async fn stage_terminal_repository_publication_input(
        &self,
        workspace: &str,
        command: &awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_types::RunError> {
        command
            .intent
            .validate()
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let input = &command.intent.input;
        let fence = ResourceOperationFence::RepositoryPublication {
            command: Box::new(command.clone()),
            lease: lease.clone(),
        };
        let staged = self
            .stage_resolved_input_with_resource_authority(
                workspace,
                input,
                Some(&fence),
                None,
                None,
            )
            .await?;
        if staged.repositories.len() != 1
            || staged.binding_checks.len() != 1
            || !matches!(
                staged.binding_checks.first(),
                Some(crate::staged_resources::ResourceBindingCheck::Repository { .. })
            )
        {
            return Err(RunError::internal(
                "terminal Repository publication did not compile one exact activation",
            ));
        }
        Ok(staged)
    }
    async fn stage_resolved_input_with_resource_authority(
        &self,
        workspace: &str,
        input: &session_resource::ResolvedInput,
        resource_authority: Option<&ResourceOperationFence>,
        realization: Option<(&str, u64, &session_resource::SessionRealizationLease)>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_types::RunError> {
        use awaken_session_configuration_contract::resource::ResolvedInputSource;
        use awaken_session_types::RunError;

        let claim = resource_authority.and_then(|authority| match authority {
            ResourceOperationFence::Run(claim) => Some(claim),
            _ => None,
        });
        let mut staged = crate::staged_resources::StagedResources::default();
        staged
            .resource_prompts
            .push(ResourcePromptProjection::try_from_input(input)?);
        let (mut projected_mount, projected_repository_access) =
            match resolved_input_layout_projection(input) {
                ManagedLayoutProjection::Mount(mount) => (Some(mount), None),
                ManagedLayoutProjection::Repository { access, .. } => (None, Some(access)),
            };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                // Ordinary staging already chose the exact Run or Session fence.
                // Terminal publication admits only Repository inputs above.
                let content = self
                    .file_content_source
                    .read(
                        workspace,
                        file_id.as_str(),
                        &awaken_resource_contract::FileReadPurpose::SessionResource,
                        resource_authority,
                    )
                    .await
                    .map_err(crate::realization_error::RealizationError::from)?
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` not found in this workspace"
                        ))
                    })?;
                let actual = awaken_resource_contract::content_id(&content.bytes);
                if actual != content.content_id {
                    return Err(RunError::bad_request(format!(
                        "file resource `{file_id}` content hash mismatch (realized `{actual}`)"
                    )));
                }
                staged.mounts.push(
                    projected_mount
                        .take()
                        .expect("File input has projected mount metadata")
                        .requirement(awaken_provisioning_contract::MountSource::InlineBytes {
                            contents: content.bytes,
                            content_hash: Some(content.content_id),
                        }),
                );
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => {
                let materialization_reference = match (
                    self.fetches_resources_upstream,
                    claim,
                    realization,
                ) {
                    (true, _, Some(_)) => {
                        let Some(ResourceOperationFence::SessionMaterialization(intent)) =
                            resource_authority
                        else {
                            return Err(RunError::internal(
                                "remote Memory realization lost its admitted Resource fence",
                            ));
                        };
                        Some(
                            awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                                awaken_session_realization_contract::SessionResourceMaterializationIntent,
                            >::encode(
                                self.memory_reference_encoder.as_deref()
                                    .ok_or_else(|| {
                                        RunError::internal(
                                            "remote Memory materialization encoder is not configured",
                                        )
                                    })?,
                                workspace,
                                memory_store_id.as_str(),
                                config.version,
                                input.access,
                                intent,
                            )
                            .map_err(crate::realization_error::RealizationError::from)?,
                        )
                    }
                    (true, Some(claim), None) => Some(
                        awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                            awaken_dispatch_contract::RunClaim,
                        >::encode(
                            self.memory_reference_encoder.as_deref().ok_or_else(|| {
                                RunError::internal(
                                    "remote Memory materialization encoder is not configured",
                                )
                            })?,
                            workspace,
                            memory_store_id.as_str(),
                            config.version,
                            input.access,
                            claim,
                        )
                        .map_err(crate::realization_error::RealizationError::from)?,
                    ),
                    (true, None, None) => {
                        return Err(RunError::bad_request(
                            "remote Memory materialization requires a dispatch claim or Session realization lease",
                        ));
                    }
                    (false, _, _) => None,
                };
                if materialization_reference.is_none() {
                    self.verify_memory_binding(workspace, memory_store_id.as_str(), config.version)
                        .await?;
                    staged.binding_checks.push(
                        crate::staged_resources::ResourceBindingCheck::MemoryStore {
                            memory_store_id: memory_store_id.to_string(),
                            config_version: config.version,
                        },
                    );
                }
                // The worker realizes one governed store directory through its
                // MemoryMounter. Resources never receives a principal,
                // role, API key, or policy: the outer authorization/ACL seam has
                // already selected workspace, store, and maximum access.
                staged.mounts.push(
                    projected_mount
                        .take()
                        .expect("Memory input has projected mount metadata")
                        .requirement(awaken_provisioning_contract::MountSource::MemoryStore {
                            store_id: memory_store_id.to_string(),
                            materialization_reference,
                            write_consistency:
                                awaken_provisioning_contract::projected_memory_write_consistency(
                                    managed_mount_access(input.access),
                                    environment.map(|projection| &projection.idle_retention),
                                ),
                        }),
                );
            }
            ResolvedInputSource::Repository {
                repository_id,
                config,
                credential: credential_pin,
            } => {
                let mount_access = projected_repository_access
                    .expect("Repository input has Repository layout metadata");
                // Historical aggregates may carry a path that predates the
                // current provider profile. Reject the exact durable value
                // before verifier, credential, transport, or Git work.
                awaken_provisioning_contract::validate_repository_mount_path(&input.mount_path)
                    .map_err(|error| {
                        RunError::bad_request(format!(
                            "repository `{repository_id}` cannot be realized: {error}"
                        ))
                    })?;
                let transport = match (resource_authority, self.fetches_resources_upstream) {
                    (Some(ResourceOperationFence::RepositoryPublication { .. }), false) => {
                        Ok(awaken_resource_contract::RepositoryTransport::Direct)
                    }
                    (authority, _) => {
                        self.repository_binding_verifier
                            .as_ref()
                            .ok_or_else(|| {
                                RunError::bad_request(
                                    "repository resources require a configured binding verifier",
                                )
                            })?
                            .verify(workspace, repository_id.as_str(), config.version, authority)
                            .await
                    }
                }
                .map_err(repository_binding_run_error)?;
                staged.binding_checks.push(
                    crate::staged_resources::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
                        remote_url: config.remote_url.clone(),
                        credential_binding: config.credential_binding.clone(),
                        authority: resource_authority.cloned().map(Box::new),
                    },
                );
                // Coordinator staging has no Run claim and cannot authorize a
                // Platform-held Git transport. Retain the exact binding check
                // and immutable Session input; the claiming Worker recompiles
                // it through the existing Gateway authorizer. No executable
                // direct Repository activation may escape this stage.
                if resource_authority.is_none()
                    && let (Some(binding), Some(pin)) = (&config.credential_binding, credential_pin)
                    && pin.selected_plaintext_holder.boundary
                        == awaken_agent_contract::credential::PlaintextBoundary::Platform
                {
                    pin.validate_for_repository(binding, &config.remote_url)
                        .map_err(|error| {
                            RunError::bad_request(format!(
                                "repository `{repository_id}` credential: {error}"
                            ))
                        })?;
                    return Ok(staged);
                }
                let (transport_url, credential_pin) = match (
                    &config.credential_binding,
                    credential_pin,
                ) {
                    (Some(binding), Some(pin)) => {
                        pin.validate_for_repository(binding, &config.remote_url)
                            .map_err(|error| {
                                RunError::bad_request(format!(
                                    "repository `{repository_id}` credential: {error}"
                                ))
                            })?;
                        match pin.selected_plaintext_holder.boundary {
                            awaken_agent_contract::credential::PlaintextBoundary::Worker => {
                                return Err(RunError::bad_request(format!(
                                    "repository `{repository_id}` credential cannot be held by the Runtime Host or Worker"
                                )));
                            }
                            awaken_agent_contract::credential::PlaintextBoundary::Platform => match transport {
                                awaken_resource_contract::RepositoryTransport::GatewayMediated {
                                    remote_url,
                                    ..
                                } => (remote_url, Some(pin.as_ref().clone())),
                                awaken_resource_contract::RepositoryTransport::Direct => {
                                    return Err(RunError::bad_request(format!(
                                        "repository `{repository_id}` Platform credential requires Gateway mediation"
                                    )));
                                }
                            },
                            awaken_agent_contract::credential::PlaintextBoundary::Workload => {
                                if !matches!(transport, awaken_resource_contract::RepositoryTransport::Direct) {
                                    return Err(RunError::bad_request(format!(
                                        "repository `{repository_id}` Sandbox credential cannot use a mediated transport"
                                    )));
                                }
                                (config.remote_url.clone(), Some(pin.as_ref().clone()))
                            }
                        }
                    }
                    (None, None) => match transport {
                        awaken_resource_contract::RepositoryTransport::Direct => {
                            (config.remote_url.clone(), None)
                        }
                        awaken_resource_contract::RepositoryTransport::GatewayMediated {
                            ..
                        } => {
                            return Err(RunError::bad_request(format!(
                                "repository `{repository_id}` has a mediated transport without a credential pin"
                            )));
                        }
                    },
                    (Some(_), None) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` credential binding has no exact Session pin"
                        )));
                    }
                    (None, Some(_)) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` has a credential pin without a binding"
                        )));
                    }
                };
                let plan = awaken_provisioning_contract::RepositoryRealizationPlan {
                    repository_id: repository_id.to_string(),
                    // `ResolvedInput.mount_path` is the durable Managed-wire truth.
                    // Never turn `/repo` into `/workspace/repo` in an ephemeral
                    // adapter projection: either every provider supports the exact
                    // path or the shared realization contract rejects it.
                    mount_path: input.mount_path.clone(),
                    source_remote_url: config.remote_url.clone(),
                    transport_url,
                    initial_branch: config.initial_branch.clone(),
                    initial_commit: config.initial_commit.clone(),
                    access: mount_access,
                };
                plan.validate_mount_path().map_err(|error| {
                    RunError::bad_request(format!(
                        "repository `{repository_id}` cannot be realized: {error}"
                    ))
                })?;
                staged
                    .repositories
                    .push(crate::staged_resources::RepositoryActivation {
                        plan,
                        credential_pin,
                    });
            }
        }
        Ok(staged)
    }
    /// One live Memory binding verification used at both staging and activation.
    pub async fn verify_memory_binding(
        &self,
        workspace: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_session_types::RunError> {
        self.resource_validator
            .as_ref()
            .ok_or_else(|| {
                awaken_session_types::RunError::bad_request(
                    "memory resources require a configured resource binding validator",
                )
            })?
            .verify_memory_binding(workspace, id, version)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))
    }
}
