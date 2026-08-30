//! Pure projection from frozen Managed resource inputs to Agent-visible paths and
//! prompt fragments. Keeping this boundary separate prevents protocol path rules
//! from being reimplemented by individual Sandbox adapters.

fn resolved_non_memory_prompt(input: &awaken_session_contract::ResolvedInput) -> Option<String> {
    use awaken_resource_contract::ResourceAccess;
    use awaken_session_contract::ResolvedInputSource;

    let access = match input.access {
        ResourceAccess::ReadOnly => "read-only",
        ResourceAccess::ReadWrite => "read/write",
    };
    let base = match &input.source {
        ResolvedInputSource::File { .. } => {
            let carried_path = managed_file_mount_path(&input.mount_path);
            format!("A file is mounted read-only at `{carried_path}`.")
        }
        ResolvedInputSource::MemoryStore { .. } => return None,
        ResolvedInputSource::Repository { .. } => format!(
            "A git repository is checked out at `{}` ({access}); use git there to read, edit, commit, and export a reviewable patch. Remote publication requires an explicit operator workflow.",
            input.mount_path
        ),
    };
    Some(match &input.instructions {
        Some(instructions) if !instructions.is_empty() => format!("{base}\n{instructions}"),
        _ => base,
    })
}

fn resolved_memory_prompts(
    input: &awaken_session_contract::ResolvedInput,
) -> crate::provisioning::MemoryPromptProjection {
    use awaken_resource_contract::ResourceAccess;

    let (access, filesystem_usage, semantic_usage) = match input.access {
        ResourceAccess::ReadOnly => (
            "read-only",
            "Use standard file tools to read it.",
            "Use `list_memories` and `read_memory`; mutation tools will reject this binding.",
        ),
        ResourceAccess::ReadWrite => (
            "read/write",
            "Use standard file tools to read and maintain it.",
            "Use `list_memories` and `read_memory` before a conditional write or delete.",
        ),
    };
    let path = managed_resource_mount_path(&input.mount_path);
    let usage = input
        .instructions
        .as_deref()
        .filter(|instructions| !instructions.is_empty())
        .map_or(String::new(), |instructions| format!("\n{instructions}"));
    let binding = input.binding_id.as_str();
    crate::provisioning::MemoryPromptProjection {
        filesystem: format!(
            "Persistent memory store `{binding}` is mounted {access} at `{path}`. {filesystem_usage}{usage}"
        ),
        semantic_tools: format!(
            "Persistent memory store `{binding}` is available {access} through the memory tools. Pass `binding: \"{binding}\"` on every memory operation. {semantic_usage}{usage}"
        ),
    }
}

pub(super) fn managed_file_mount_path(requested: &str) -> String {
    let logical = requested.trim_start_matches('/');
    if logical.starts_with("mnt/session/uploads/") {
        format!("/{logical}")
    } else {
        format!("/mnt/session/uploads/{logical}")
    }
}

pub(super) fn managed_resource_mount_path(requested: &str) -> String {
    let logical = requested.trim_start_matches('/');
    if logical.starts_with("mnt/") {
        format!("/{logical}")
    } else {
        format!("/mnt/{logical}")
    }
}

type TerminalRepositoryPublicationFence = (
    awaken_session_contract::SessionRepositoryPublicationCommand,
    awaken_session_contract::SessionRealizationLease,
);

/// Select only the Repository authorization port while keeping the resource
/// projection itself singular. Terminal authority is interpreted together with
/// the Host's existing upstream topology fact: local effects use the frozen
/// command directly, while a remote effect requires its lease verifier. A
/// remote Run without its claim still fails closed.
#[derive(Clone, Copy)]
enum RepositoryProjectionAuthority<'a> {
    Run(Option<&'a awaken_dispatch::RunClaim>),
    Terminal(Option<&'a TerminalRepositoryPublicationFence>),
}

impl<'a> RepositoryProjectionAuthority<'a> {
    fn run_claim(self) -> Option<&'a awaken_dispatch::RunClaim> {
        match self {
            Self::Run(claim) => claim,
            Self::Terminal(_) => None,
        }
    }
}

impl crate::ManagedHost {
    /// Compile one frozen Managed input into the provisioning vocabulary. This
    /// projection owns protocol paths and secret-free credential pins; the
    /// Repository effect edge owns the one typed material translation. Sandbox
    /// adapters consume only the resulting neutral requirements.
    pub(super) async fn stage_resolved_input(
        &self,
        workspace: &str,
        input: &awaken_session_contract::ResolvedInput,
        claim: Option<&awaken_dispatch::RunClaim>,
    ) -> Result<crate::provisioning::StagedResources, awaken_session_contract::RunError> {
        self.stage_resolved_input_with_repository_authority(
            workspace,
            input,
            RepositoryProjectionAuthority::Run(claim),
        )
        .await
    }

    /// Compile the exact Repository input carried by a terminal publication
    /// command through the same path as ordinary Session staging. A remote lease
    /// selects the HTTP authority verifier; a local command needs no mutable
    /// catalog re-read. Path, access, credential pin, transport, and
    /// realization-plan projection stay canonical here.
    pub(super) async fn stage_terminal_repository_publication_input(
        &self,
        workspace: &str,
        command: &awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> Result<crate::provisioning::StagedResources, awaken_session_contract::RunError> {
        command
            .intent
            .validate()
            .map_err(|error| awaken_session_contract::RunError::bad_request(error.to_string()))?;
        let input = &command.intent.input;
        if !matches!(
            input.source,
            awaken_session_contract::ResolvedInputSource::Repository { .. }
        ) {
            return Err(awaken_session_contract::RunError::bad_request(
                "terminal Repository publication input is not a Repository",
            ));
        }
        let fence = lease.map(|lease| (command.clone(), lease.clone()));
        self.stage_resolved_input_with_repository_authority(
            workspace,
            input,
            RepositoryProjectionAuthority::Terminal(fence.as_ref()),
        )
        .await
    }

    async fn stage_resolved_input_with_repository_authority(
        &self,
        workspace: &str,
        input: &awaken_session_contract::ResolvedInput,
        repository_authority: RepositoryProjectionAuthority<'_>,
    ) -> Result<crate::provisioning::StagedResources, awaken_session_contract::RunError> {
        use awaken_resource_contract::ResourceAccess;
        use awaken_session_contract::{ResolvedInputSource, RunError};

        let claim = repository_authority.run_claim();
        let mut staged = crate::provisioning::StagedResources::default();
        let logical = input.mount_path.trim_start_matches('/').to_string();
        if matches!(input.source, ResolvedInputSource::MemoryStore { .. }) {
            staged.memory_prompts.push(resolved_memory_prompts(input));
        } else {
            staged.prompts.push(
                resolved_non_memory_prompt(input)
                    .expect("non-Memory input has one prompt projection"),
            );
        }
        let mount_access = match input.access {
            ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
            ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
        };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                let content = self
                    .host
                    .file_content_source
                    .read(
                        workspace,
                        file_id.as_str(),
                        &awaken_resource_contract::FileReadPurpose::SessionResource,
                        claim,
                    )
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
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
                let managed_path = managed_file_mount_path(&input.mount_path);
                staged
                    .mounts
                    .push(awaken_provisioning_contract::MountRequirement {
                        mount_id: file_id.to_string(),
                        source: awaken_provisioning_contract::MountSource::InlineBytes {
                            contents: content.bytes,
                            content_hash: Some(content.content_id),
                        },
                        mount_path: managed_path,
                        access: awaken_provisioning_contract::MountAccess::ReadOnly,
                        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                        required: true,
                    });
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => {
                let materialization_reference = match (&self.host.upstream, claim) {
                    (Some(_), Some(claim)) => Some(
                        self.host
                            .memory_reference_encoder
                            .as_ref()
                            .ok_or_else(|| {
                                RunError::internal(
                                    "remote Memory materialization encoder is not configured",
                                )
                            })?
                            .encode(
                                workspace,
                                memory_store_id.as_str(),
                                config.version,
                                input.access,
                                claim,
                            )
                            .map_err(|error| RunError::internal(error.to_string()))?,
                    ),
                    (Some(_), None) => {
                        return Err(RunError::bad_request(
                            "remote Memory materialization requires a dispatch claim",
                        ));
                    }
                    (None, _) => None,
                };
                if materialization_reference.is_none() {
                    let validator = self.resource_validator.as_ref().ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?;
                    validator
                        .verify_memory_binding(workspace, memory_store_id.as_str(), config.version)
                        .map_err(|error| RunError::bad_request(error.to_string()))?;
                    staged.binding_checks.push(
                        crate::provisioning::ResourceBindingCheck::MemoryStore {
                            memory_store_id: memory_store_id.to_string(),
                            config_version: config.version,
                        },
                    );
                }
                // The worker realizes one governed store directory through its
                // MemoryMounter. Resources never receives a principal,
                // role, API key, or policy: the outer authorization/ACL seam has
                // already selected workspace, store, and maximum access.
                staged
                    .mounts
                    .push(awaken_provisioning_contract::MountRequirement {
                    mount_id: input.binding_id.to_string(),
                    source: awaken_provisioning_contract::MountSource::MemoryStore {
                        store_id: memory_store_id.to_string(),
                        materialization_reference,
                        write_consistency:
                            awaken_provisioning_contract::MemoryWriteConsistency::ProviderDefault,
                    },
                    mount_path: managed_resource_mount_path(&logical),
                    access: mount_access,
                    lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                    required: true,
                });
            }
            ResolvedInputSource::Repository {
                repository_id,
                config,
                credential: credential_pin,
            } => {
                let transport = match (repository_authority, &self.host.upstream) {
                    (RepositoryProjectionAuthority::Terminal(Some(fence)), Some(_)) => self
                            .repository_publication_binding_verifier
                            .as_ref()
                            .ok_or_else(|| {
                                RunError::bad_request(
                                    "remote terminal Repository publication requires a configured binding verifier",
                                )
                            })?
                            .verify(
                                workspace,
                                repository_id.as_str(),
                                config.version,
                                Some(fence),
                            )
                            .await,
                    (RepositoryProjectionAuthority::Terminal(None), Some(_)) => Err(
                        awaken_resource_contract::RepositoryBindingVerifierError::new(
                            "remote terminal Repository publication requires its realization lease",
                        ),
                    ),
                    (RepositoryProjectionAuthority::Terminal(_), None) => {
                        Ok(awaken_resource_contract::RepositoryTransport::Direct)
                    }
                    (RepositoryProjectionAuthority::Run(claim), _) => self
                        .repository_binding_verifier
                        .as_ref()
                        .ok_or_else(|| {
                            RunError::bad_request(
                                "repository resources require a configured binding verifier",
                            )
                        })?
                        .verify(workspace, repository_id.as_str(), config.version, claim)
                        .await,
                }
                .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
                        remote_url: config.remote_url.clone(),
                        credential_binding: config.credential_binding.clone(),
                        claim: claim.cloned(),
                    });
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
                            awaken_runtime_contract::PlaintextBoundary::Worker => {
                                if !matches!(transport, awaken_resource_contract::RepositoryTransport::Direct) {
                                    return Err(RunError::bad_request(format!(
                                        "repository `{repository_id}` Worker credential cannot use a mediated transport"
                                    )));
                                }
                                (config.remote_url.clone(), Some(pin.as_ref().clone()))
                            }
                            awaken_runtime_contract::PlaintextBoundary::Platform => match transport {
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
                            awaken_runtime_contract::PlaintextBoundary::Workload => {
                                return Err(RunError::bad_request(format!(
                                    "repository `{repository_id}` credential requires an unsupported plaintext holder"
                                )));
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
                staged
                    .repositories
                    .push(crate::provisioning::RepositoryActivation {
                        plan: awaken_provisioning_contract::RepositoryRealizationPlan {
                            repository_id: repository_id.to_string(),
                            mount_path: logical,
                            source_remote_url: config.remote_url.clone(),
                            transport_url,
                            initial_branch: config.initial_branch.clone(),
                            initial_commit: config.initial_commit.clone(),
                            access: mount_access,
                        },
                        credential_pin,
                    });
            }
        }
        Ok(staged)
    }
}



impl ManagedHost {

    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) -> Self {
        self.resource_validator = Some(validator);
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Registry database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<dyn RepositoryBindingVerifier<awaken_dispatch::RunClaim>>,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self
    }

    /// Install the same Repository binding boundary under terminal Session
    /// publication authority. Remote implementations require the exact durable
    /// command plus realization lease; local registry implementations reuse the
    /// generic verifier and therefore retain one Resource/config validation path.
    #[must_use]
    pub fn with_repository_publication_binding_verifier(
        mut self,
        verifier: Arc<
            dyn RepositoryBindingVerifier<(
                awaken_session_contract::SessionRepositoryPublicationCommand,
                awaken_session_contract::SessionRealizationLease,
            )>,
        >,
    ) -> Self {
        self.repository_publication_binding_verifier = Some(verifier);
        self
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// configured here.
    pub(super) async fn compile_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_dispatch::RunClaim>,
    ) -> Result<CompiledEffectiveInputs, RunError> {
        let mut all = crate::provisioning::StagedResources::default();
        let mut memory_bindings = std::collections::HashMap::new();
        for input in inputs.inputs() {
            let one = self.stage_resolved_input(workspace, input, claim).await?;
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
                .compile_memory_binding(thread, workspace, input, materialization_reference)
                .await?
            {
                memory_bindings.insert(binding_id, memory);
            }
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.memory_prompts.extend(one.memory_prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
        }

        Ok(CompiledEffectiveInputs {
            staged: all,
            memory_bindings,
        })
    }

    /// Compile the one Memory-specific leaf shared by ordinary Session staging
    /// and post-commit recovery from a frozen dispatch. The caller owns Resource
    /// selection and may install the result into a resident Session slot; this
    /// leaf only binds one already-resolved input and never opens an Environment.
    pub(super) async fn compile_memory_binding(
        &self,
        session_thread: &str,
        workspace: &str,
        input: &awaken_session_contract::ResolvedInput,
        materialization_reference: Option<String>,
    ) -> Result<Option<(String, Arc<crate::memory::BoundMemory>)>, RunError> {
        let awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } = &input.source
        else {
            return Ok(None);
        };
        let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
        if let Some(reference) = &materialization_reference {
            // Remote Memory claim decision table: active + exact config =>
            // snapshot preflight succeeds; archived/config-changed/stale claim
            // fails before the mounter reuses a prior projection.
            self.host
                .memory_repository()
                .snapshot_heads(reference)
                .await
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        let handle = self.host.platform_memory_handle(
            materialization_reference
                .clone()
                .unwrap_or_else(|| memory_store_id.to_string()),
            writable,
        );
        let resource_validator = if materialization_reference.is_some() {
            None
        } else {
            Some(
                self.resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?
                    .clone(),
            )
        };
        Ok(Some((
            input.binding_id.to_string(),
            Arc::new(self.host.memory.bind(
                session_thread,
                workspace,
                handle,
                resource_validator,
                config,
                writable,
            )),
        )))
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    pub(super) async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_dispatch::RunClaim>,
    ) -> Result<(), RunError> {
        let desired = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace,
            resource_revision,
            resources.clone(),
        );
        // The authority-side recovery path may first reconcile a retained or
        // pending generation and then prepare the complete Session projection.
        // Both operations carry the same canonical manifest. Avoid compiling and
        // installing it twice; claimed Workers remain excluded because every
        // claim must revalidate live Resource state even when the pin is equal.
        if claim.is_none() && self.host.thread_resource_manifest(thread).as_ref() == Some(&desired)
        {
            return Ok(());
        }
        let versions = self
            .host
            .skills
            .load_pinned(workspace, resources.skills(), claim)
            .await
            .map_err(skill_store_run_error)?;
        let compiled = self
            .compile_effective_inputs(thread, workspace, resources, claim)
            .await?;
        // Publish only after every fallible Resource/Skill read has succeeded.
        // The slot lock then exposes one complete logical Resource generation;
        // failed compilation leaves no workspace, manifest, prompt, or binding
        // residue that a later attempt could mistake for Session truth.
        self.host.register_thread_workspace(thread, workspace);
        self.install_effective_inputs(thread, workspace, desired.revision, resources, compiled)
            .await?;
        self.host
            .session_slots
            .update(thread, |slot| slot.skills = Some(versions));
        Ok(())
    }

    pub(super) async fn validate_thread_resource_bindings(&self, thread: &str) -> Result<(), RunError> {
        use crate::provisioning::ResourceBindingCheck;

        // This method is entered only through the SessionRuntime application
        // port. Preserve that neutral identity before dispatch so a claiming
        // Worker enters the frozen Session realization path.
        self.host
            .session_slots
            .update(thread, |slot| slot.session_dispatch = true);
        let checks = self
            .host
            .session_slots
            .read(thread, |slot| slot.resources.binding_checks.clone())
            .unwrap_or_default();
        if checks.is_empty() {
            return Ok(());
        }
        let workspace = self.host.thread_workspace(thread);
        for check in checks {
            match check {
                ResourceBindingCheck::MemoryStore {
                    memory_store_id,
                    config_version,
                } => self
                    .resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?
                    .verify_memory_binding(&workspace, &memory_store_id, config_version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
                ResourceBindingCheck::Repository {
                    repository_id,
                    config_version,
                    claim,
                    ..
                } => self
                    .repository_binding_verifier
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured binding verifier",
                        )
                    })?
                    .verify(&workspace, &repository_id, config_version, claim.as_ref())
                    .await
                    .map(|_| ())
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            }
        }
        Ok(())
    }

    /// Wire runtime credential injection for the already-frozen Session bindings
    /// and Repository realization.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(PinnedCredentialMaterializer::new(credentials, secrets))
    }

    /// Reuse the process startup's canonical exact materializer for MCP and
    /// Repository realization instead of constructing a peer over the same stores.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: PinnedCredentialMaterializer,
    ) -> Self {
        self.credentials = Some(materializer);
        self
    }

    /// Install the credential adapter's exact OAuth refresh port. The Host
    /// retains only this factory and never receives Credential/Secret Store
    /// handles.
    #[must_use]
    pub fn with_credential_refresh_factory(
        mut self,
        factory: Arc<dyn CredentialRefreshFactory>,
    ) -> Self {
        self.credential_refresh_factory = Some(factory);
        self
    }

    /// Replace the local Host MCP realization adapter with one downstream
    /// implementation of the same exact-generation Session port. This is the
    /// sole injection seam used by durable Worker commands; desired state and
    /// credential selection remain outside the implementation.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self
    }}

use super::*;
