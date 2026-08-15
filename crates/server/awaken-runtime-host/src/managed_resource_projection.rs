//! Pure projection from frozen Managed resource inputs to Agent-visible paths and
//! prompt fragments. Keeping this boundary separate prevents protocol path rules
//! from being reimplemented by individual Sandbox adapters.

pub(super) fn resolved_resource_prompt(input: &awaken_session_contract::ResolvedInput) -> String {
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
        ResolvedInputSource::MemoryStore { .. } => {
            let carried_path = managed_resource_mount_path(&input.mount_path);
            format!("A persistent memory store is mounted {access} at `{carried_path}`.")
        }
        ResolvedInputSource::Repository { .. } => format!(
            "A git repository is checked out at `{}` ({access}); use git there to read, edit, commit, and push.",
            input.mount_path
        ),
    };
    match &input.instructions {
        Some(instructions) if !instructions.is_empty() => format!("{base}\n{instructions}"),
        _ => base,
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

fn repository_http_basic_credential(
    material: awaken_runtime_contract::CredentialMaterial,
) -> Result<awaken_provisioning_contract::RepositoryHttpBasicCredential, &'static str> {
    let awaken_runtime_contract::CredentialMaterial::Structured(mut material) = material else {
        return Err("HTTP Basic requires structured credential material");
    };
    if material.type_id != awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE {
        return Err("HTTP Basic credential material has the wrong type");
    }
    let username = material
        .fields
        .remove("username")
        .ok_or("HTTP Basic credential material has no username")?;
    let password = material
        .fields
        .remove("password")
        .ok_or("HTTP Basic credential material has no password")?;
    Ok(awaken_provisioning_contract::RepositoryHttpBasicCredential::new(username, password))
}

impl crate::ManagedHost {
    /// Compile one frozen Managed input into the provisioning vocabulary. This
    /// projection owns protocol paths and typed credential translation; Sandbox
    /// adapters consume only the resulting neutral requirements.
    pub(super) async fn stage_resolved_input(
        &self,
        workspace: &str,
        input: &awaken_session_contract::ResolvedInput,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<crate::provisioning::StagedResources, awaken_session_contract::RunError> {
        use awaken_resource_contract::ResourceAccess;
        use awaken_session_contract::{ResolvedInputSource, RunError};

        let mut staged = crate::provisioning::StagedResources::default();
        let logical = input.mount_path.trim_start_matches('/').to_string();
        staged.prompts.push(resolved_resource_prompt(input));
        let mount_access = match input.access {
            ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
            ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
        };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                let (content_digest, bytes) = self
                    .host
                    .file_content_source
                    .read(workspace, file_id.as_str(), claim)
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` not found in this workspace"
                        ))
                    })?;
                let actual = awaken_resource_contract::content_id(&bytes);
                if actual != content_digest {
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
                            contents: bytes,
                            content_hash: Some(content_digest),
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
                        .validate_memory_binding(
                            workspace,
                            memory_store_id.as_str(),
                            config.version,
                        )
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
                let verifier = self.repository_binding_verifier.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "repository resources require a configured binding verifier",
                    )
                })?;
                verifier
                    .verify(workspace, repository_id.as_str(), config.version, claim)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
                        claim: claim.cloned(),
                    });
                let credential = match (&config.credential_binding, credential_pin) {
                    (Some(binding), Some(pin)) => {
                        pin.validate_for_binding(binding).map_err(|error| {
                            RunError::bad_request(format!(
                                "repository `{repository_id}` credential: {error}"
                            ))
                        })?;
                        if pin.selected_plaintext_holder.boundary
                            != awaken_runtime_contract::PlaintextBoundary::Worker
                        {
                            return Err(RunError::bad_request(format!(
                                "repository `{repository_id}` credential requires an unsupported plaintext holder"
                            )));
                        }
                        let credentials = self.credentials.as_ref().ok_or_else(|| {
                            RunError::bad_request(
                                "repository credential requires a configured credential vault",
                            )
                        })?;
                        let material = credentials
                            .resolve_for_workspace(
                                &pin.access,
                                &pin.selected_plaintext_holder,
                                awaken_runtime_contract::CredentialRealizationKind::WorkerRelay,
                                workspace,
                                &(repository_id, config.version),
                            )
                            .await
                            .map_err(|error| {
                                RunError::bad_request(format!(
                                    "repository `{repository_id}` credential: {error}"
                                ))
                            })?
                            .material;
                        Some(repository_http_basic_credential(material).map_err(|error| {
                            RunError::bad_request(format!(
                                "repository `{repository_id}` credential: {error}"
                            ))
                        })?)
                    }
                    (None, None) => None,
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
                            remote_url: config.remote_url.clone(),
                            initial_branch: config.initial_branch.clone(),
                            initial_commit: config.initial_commit.clone(),
                            access: mount_access,
                        },
                        credential,
                    });
            }
        }
        Ok(staged)
    }
}
