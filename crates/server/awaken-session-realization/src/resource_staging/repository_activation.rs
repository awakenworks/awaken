// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Repository activation and terminal publication run through the selected credential materializer and binding verifier; no new authority is introduced.
//! Repository activation of Resource staging: the secret-free plan a fresh
//! Environment's working tree is realized from, the ephemeral transport
//! credential each Git effect receives after authorization, and the terminal
//! Repository publication.

use super::ResourceStaging;
use crate::realization_error::{PendingState, RealizationError};
use crate::staged_resources::RepositoryPublicationActivationError;
use crate::staged_resources::{RepositoryActivation, ResourceBindingCheck};
use awaken_agent_contract::credential::PlaintextBoundary;
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_execution_contract::ResourceOperationFence;

fn repository_http_basic_credential(
    material: awaken_credential_contract::CredentialMaterial,
) -> Result<pc::RepositoryHttpBasicCredential, &'static str> {
    let awaken_credential_contract::CredentialMaterial::Structured(mut material) = material else {
        return Err("HTTP Basic requires structured credential material");
    };
    if material.type_id != awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE {
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
    Ok(pc::RepositoryHttpBasicCredential::new(username, password))
}

impl ResourceStaging {
    /// Realize a thread's resolved Repository inputs into its freshly-created
    /// environment. The narrow port receives only a secret-free plan and an ephemeral
    /// transport credential after authorization/config resolution. A failure aborts
    /// activation so the Session cannot run believing a working tree exists.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn realize_thread_repositories(
        &self,
        thread: &str,
        fallback_workspace: &str,
        realizer: &dyn pc::RepositoryRealizer,
    ) -> Result<(), RealizationError> {
        let resources = self.thread_resources_snapshot(thread)?;
        for repository in &resources.repositories {
            self.realize_repository_activation(
                thread,
                fallback_workspace,
                repository,
                &resources.binding_checks,
                realizer,
                pc::RepositoryRealizationIntent::Initialize,
            )
            .await?;
        }
        Ok(())
    }

    /// Execute one Repository clone edge through the same operation-scoped
    /// credential path used by initial activation and hot Resource replacement.
    pub async fn realize_repository_activation(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
        intent: pc::RepositoryRealizationIntent,
    ) -> Result<(), RealizationError> {
        repository
            .plan
            .validate_mount_path()
            .map_err(RealizationError::from)?;
        let credential = self
            .repository_operation_credential(
                thread,
                fallback_workspace,
                repository,
                binding_checks,
                None,
            )
            .await?;
        realizer
            .realize_repository(&repository.plan, credential.as_ref(), intent)
            .await
            .map_err(RealizationError::from)
    }

    /// Materialize only the credential required by this Git effect. A direct
    /// Sandbox-held pin repeats exact source revision, Workspace, descriptor
    /// expiry/target/material, holder, and Repository id/version binding checks.
    /// A Platform-held pin refreshes its short Gateway capability and cannot
    /// downgrade to direct material. The returned owned value is dropped after
    /// the realizer call and is never written back to `RepositoryActivation`.
    async fn repository_operation_credential(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        publication_fence: Option<&(
            awaken_session_contract::SessionRepositoryPublicationCommand,
            session_resource::SessionRealizationLease,
        )>,
    ) -> Result<Option<pc::RepositoryHttpBasicCredential>, RealizationError> {
        let pin = repository.credential_pin.as_ref();
        if pin.is_none() && publication_fence.is_none() {
            return Ok(None);
        }
        let mut matching = binding_checks.iter().filter_map(|check| match check {
            ResourceBindingCheck::Repository {
                repository_id,
                config_version,
                remote_url,
                credential_binding,
                authority,
            } if repository_id == &repository.plan.repository_id => Some((
                *config_version,
                remote_url.as_str(),
                credential_binding.as_deref(),
                authority.as_deref(),
            )),
            _ => None,
        });
        let Some((config_version, remote_url, credential_binding, authority)) = matching.next()
        else {
            return Err(RealizationError::invariant(
                "protected Repository has no exact binding check",
            ));
        };
        if matching.next().is_some() {
            return Err(RealizationError::invariant(
                "protected Repository has ambiguous binding checks",
            ));
        }
        if let Some(pin) = pin {
            let credential_binding = credential_binding.ok_or_else(|| {
                RealizationError::invariant(
                    "protected Repository credential pin has no authored binding",
                )
            })?;
            pin.validate_for_repository(credential_binding, remote_url)
                .map_err(RealizationError::from)?;
        }
        if repository.plan.source_remote_url != remote_url {
            return Err(RealizationError::invariant(
                "Repository changed its frozen source URL",
            ));
        }
        let publication_authority = publication_fence.map(|(command, lease)| {
            ResourceOperationFence::RepositoryPublication {
                command: Box::new(command.clone()),
                lease: lease.clone(),
            }
        });
        let publication_transport = match (
            publication_authority.as_ref(),
            self.fetches_resources_upstream,
        ) {
            (Some(fence), true) => {
                let verifier = self.repository_binding_verifier.clone().ok_or_else(|| {
                    RealizationError::invariant(
                        "terminal Repository publication has no binding verifier",
                    )
                })?;
                Some(
                    verifier
                        .verify(
                            &self.sessions.workspace(thread, fallback_workspace)?,
                            &repository.plan.repository_id,
                            config_version,
                            Some(fence),
                        )
                        .await
                        .map_err(RealizationError::RepositoryBinding)?,
                )
            }
            // Local topology already compiled this exact command through the
            // frozen terminal projection. It has no Gateway capability to mint
            // and must not re-read the mutable Repository Registry here.
            (Some(_), false) => Some(awaken_resource_contract::RepositoryTransport::Direct),
            (None, _) => None,
        };
        let Some(pin) = pin else {
            return match publication_transport {
                Some(awaken_resource_contract::RepositoryTransport::Direct)
                    if repository.plan.transport_url == repository.plan.source_remote_url =>
                {
                    Ok(None)
                }
                Some(awaken_resource_contract::RepositoryTransport::Direct) => {
                    Err(RealizationError::invariant(
                        "direct Repository changed its frozen transport URL",
                    ))
                }
                Some(awaken_resource_contract::RepositoryTransport::GatewayMediated { .. }) => {
                    Err(RealizationError::invariant(
                        "Repository has a mediated transport without a credential pin",
                    ))
                }
                None => Ok(None),
            };
        };
        match pin.selected_plaintext_holder.boundary {
            PlaintextBoundary::Worker => Err(RealizationError::invariant(
                "Runtime Host or Worker cannot hold a Repository transport credential",
            )),
            PlaintextBoundary::Workload => {
                if publication_transport.as_ref().is_some_and(|transport| {
                    !matches!(
                        transport,
                        awaken_resource_contract::RepositoryTransport::Direct
                    )
                }) {
                    return Err(RealizationError::invariant(
                        "Sandbox-held Repository credential cannot use a mediated transport",
                    ));
                }
                if repository.plan.transport_url != remote_url {
                    return Err(RealizationError::invariant(
                        "direct Repository changed its frozen transport URL",
                    ));
                }
                let materializer = self.credential_materializer.as_ref().ok_or_else(|| {
                    RealizationError::invariant(
                        "repository credential requires a configured credential materializer",
                    )
                })?;
                let workspace = self.sessions.workspace(thread, fallback_workspace)?;
                let material = materializer
                    .resolve_for_workspace(
                        &pin.access,
                        &pin.selected_plaintext_holder,
                        awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
                        &workspace,
                        &(&repository.plan.repository_id, config_version),
                    )
                    .await
                    .map_err(RealizationError::from)?
                    .material;
                repository_http_basic_credential(material)
                    .map(Some)
                    .map_err(RealizationError::invariant)
            }
            PlaintextBoundary::Platform => {
                let workspace = self.sessions.workspace(thread, fallback_workspace)?;
                let transport = match publication_transport {
                    Some(transport) => transport,
                    None => {
                        let verifier =
                            self.repository_binding_verifier.clone().ok_or_else(|| {
                                RealizationError::invariant(
                                    "Gateway-mediated Repository has no binding verifier",
                                )
                            })?;
                        verifier
                            .verify(
                                &workspace,
                                &repository.plan.repository_id,
                                config_version,
                                authority,
                            )
                            .await
                            .map_err(RealizationError::RepositoryBinding)?
                    }
                };
                match transport {
                    awaken_resource_contract::RepositoryTransport::GatewayMediated {
                        remote_url,
                        capability,
                        ..
                    } if remote_url == repository.plan.transport_url => {
                        Ok(Some(pc::RepositoryHttpBasicCredential::gateway_capability(
                            capability.expose().to_owned(),
                        )))
                    }
                    awaken_resource_contract::RepositoryTransport::GatewayMediated { .. } => {
                        Err(RealizationError::invariant(
                            "Gateway-mediated Repository changed its frozen remote URL",
                        ))
                    }
                    awaken_resource_contract::RepositoryTransport::Direct => {
                        Err(RealizationError::invariant(
                            "Gateway-mediated Repository cannot fall back to direct credentials",
                        ))
                    }
                }
            }
        }
    }

    /// Publish one exact, already-compiled Repository activation. The caller
    /// supplies the activation and binding checks derived from the frozen intent;
    /// this method never selects from the thread's current Resource set and never
    /// loops over unrelated repositories. Credential realization remains the same
    /// operation-scoped edge used by clone and replacement.
    // Keep exact effect coordinates and the deferred Workspace fallback explicit.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_repository_activation(
        &self,
        thread: &str,
        fallback_workspace: &str,
        repository: &RepositoryActivation,
        binding_checks: &[ResourceBindingCheck],
        realizer: &dyn pc::RepositoryRealizer,
        expectation: &pc::RepositoryPublicationExpectation,
        publication_fence: Option<&(
            awaken_session_contract::SessionRepositoryPublicationCommand,
            session_resource::SessionRealizationLease,
        )>,
    ) -> Result<pc::RepositoryPublicationReceipt, RepositoryPublicationActivationError> {
        repository
            .plan
            .validate_mount_path()
            .map_err(|error| RepositoryPublicationActivationError::Failed(error.into()))?;
        expectation
            .validate()
            .map_err(|error| RepositoryPublicationActivationError::Failed(error.into()))?;
        if repository.plan.access == pc::MountAccess::ReadOnly {
            return Err(RepositoryPublicationActivationError::Failed(
                RealizationError::invariant("read-only repository cannot be published"),
            ));
        }
        let credential = self
            .repository_operation_credential(
                thread,
                fallback_workspace,
                repository,
                binding_checks,
                publication_fence,
            )
            .await
            .map_err(RepositoryPublicationActivationError::Failed)?;
        let receipt = realizer
            .publish_repository(&repository.plan, expectation, credential.as_ref())
            .await
            .map_err(|error| match error {
                pc::RepositoryPublicationError::Rejected(rejection) => {
                    RepositoryPublicationActivationError::Rejected(rejection)
                }
                pc::RepositoryPublicationError::Unavailable(error) => {
                    RepositoryPublicationActivationError::Failed(RealizationError::pending(
                        PendingState::RepositoryPublicationTransportUnavailable,
                        error.to_string(),
                    ))
                }
            })?;
        receipt
            .verify(&repository.plan, expectation)
            .map_err(|error| {
                RepositoryPublicationActivationError::Failed(RealizationError::from(error))
            })?;
        Ok(receipt)
    }

    pub fn validate_repository_environment_adoption_paths(
        &self,
        provider: &crate::session_environment::SandboxProviderRoles,
        spec: &pc::SandboxSpec,
        repository_paths: &[&str],
        historical_owned_paths: &[&str],
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let effective = provider
            .realizer()
            .effective_spec(spec)
            .map_err(RealizationError::from)?;
        pc::validate_repository_sandbox_adoption_layout(
            repository_paths,
            &effective,
            historical_owned_paths,
        )
        .map_err(RealizationError::from)?;
        Ok(effective)
    }
}
