//! Pure projection from frozen Managed resource inputs to Agent-visible paths and
//! prompt fragments. Keeping this boundary separate prevents protocol path rules
//! from being reimplemented by individual Sandbox adapters.

pub(super) fn resolved_resource_prompt(input: &awaken_protocol_managed::ResolvedInput) -> String {
    use crate::awaken_resource_contract::ResourceAccess;
    use awaken_protocol_managed::ResolvedInputSource;

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
            let carried_path = format!(".mnt/{}", input.mount_path.trim_start_matches('/'));
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
        input: &awaken_protocol_managed::ResolvedInput,
    ) -> Result<crate::provisioning::StagedResources, awaken_protocol_managed::RunError> {
        use crate::awaken_resource_contract::ResourceAccess;
        use awaken_protocol_managed::{ResolvedInputSource, RunError};

        let mut staged = crate::provisioning::StagedResources::default();
        let logical = input.mount_path.trim_start_matches('/').to_string();
        staged.prompts.push(resolved_resource_prompt(input));
        let mount_access = match input.access {
            ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
            ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
        };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                let record = self
                    .host
                    .file_record(workspace, file_id.as_str())
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` not found in this workspace"
                        ))
                    })?;
                let bytes = self
                    .host
                    .file_store()
                    .get(&record.blob_id)
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` references a missing blob"
                        ))
                    })?;
                let actual = awaken_sandbox_local::content_fingerprint(&bytes);
                if actual != record.blob_id {
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
                            content_hash: Some(record.blob_id),
                        },
                        mount_path: managed_path,
                        access: awaken_provisioning_contract::MountAccess::ReadOnly,
                        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                        required: true,
                    });
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::File {
                        file_id: file_id.to_string(),
                    });
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => {
                let validator = self.resource_validator.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "memory resources require a configured resource binding validator",
                    )
                })?;
                validator
                    .validate_memory_binding(workspace, memory_store_id.as_str(), config.version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged.binding_checks.push(
                    crate::provisioning::ResourceBindingCheck::MemoryStore {
                        memory_store_id: memory_store_id.to_string(),
                        config_version: config.version,
                    },
                );
                // The worker realizes one governed store directory through its
                // MemoryMounter. The resource plane never receives a principal,
                // role, API key, or policy: the outer authorization/ACL seam has
                // already selected workspace, store, and maximum access.
                staged
                    .mounts
                    .push(awaken_provisioning_contract::MountRequirement {
                    mount_id: input.binding_id.to_string(),
                    source: awaken_provisioning_contract::MountSource::MemoryStore {
                        store_id: memory_store_id.to_string(),
                        write_consistency:
                            awaken_provisioning_contract::MemoryWriteConsistency::ProviderDefault,
                    },
                    mount_path: format!(".mnt/{logical}"),
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
                let validator = self.resource_validator.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "repository resources require a configured resource binding validator",
                    )
                })?;
                validator
                    .validate_repository_binding(workspace, repository_id.as_str(), config.version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
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
