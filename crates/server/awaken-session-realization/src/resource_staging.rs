// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Resource staging owns the original projection mutex and cancellation/cache fields under the canonical installed-slot lock; publication still checks the existing realization proof.
//! Resource staging of the realization executor: the frozen Session Resources
//! as the Environment receives them. It projects the provider-effective sandbox
//! spec, registers the staged Resources and the active manifest, and validates
//! the managed mount layout against a provider.

use crate::memory_binding::MemoryBinding;
use crate::realization_error::RealizationError;
use crate::sandbox_projection::{
    ProjectedSandboxLayout, project_session_mounts, resource_mount_is_create_time,
};
use crate::staged_resources::StagedResources;
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_configuration_contract::environment_projection;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;
use std::sync::Arc;

pub mod preparation;
pub mod repository_activation;
pub mod skills;
pub use preparation::ResourcePreparation;

/// Stages one installed Session's Resources into its Environment spec; its only
/// state is the installed Sessions' realization layer it reads and records into.
/// Its exact Skill byte source is selected before construction, without catalog writes.
#[derive(Clone)]
pub struct ResourceStaging {
    pub sessions: crate::session_realizations::SessionRealizations,
    /// Exact immutable bytes only; this grants no catalog/authoring access.
    pub bundle_source: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
    pub file_content_source:
        Arc<dyn awaken_resource_contract::FileContentSource<ResourceOperationFence>>,
    pub memory: awaken_resource_contract::MemoryExecutionContent,
    pub resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    pub repository_binding_verifier: Option<
        Arc<dyn awaken_resource_contract::RepositoryBindingVerifier<ResourceOperationFence>>,
    >,
    pub fetches_resources_upstream: bool,
    /// The same exact materializer selected for the Host, used only at a Git effect.
    pub credential_materializer:
        Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    pub memory_reference_encoder:
        Option<Arc<dyn crate::memory_binding::RuntimeMemoryReferenceEncoder>>,
}

impl ResourceStaging {
    pub fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_provisioning_contract::MountRequirement>, RealizationError> {
        Ok(self
            .sessions
            .modify_session(thread, |slot| {
                crate::sandbox_projection::project_session_mounts(
                    slot.realization.staging.resources.mounts.clone(),
                    slot.inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice()),
                    slot.realization.staging.content_delivery,
                )
            })?
            .unwrap_or_default())
    }

    pub fn register_thread_memory(
        &self,
        thread: &str,
        memory: Option<Arc<MemoryBinding>>,
    ) -> Result<(), RealizationError> {
        self.sessions.update_realization(thread, |r| {
            r.staging.memory_bindings.clear();
            r.staging.memory = memory;
        })?;
        Ok(())
    }

    /// Select the exact Awaken extension binding authored by the published
    /// Agent. A missing binding clears any prior selection before failing, so a
    /// rebuilt context cannot retain stale automatic-memory authority.
    pub fn select_thread_memory_binding(
        &self,
        thread: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<Arc<MemoryBinding>>, RealizationError> {
        let selected = self.sessions.update_realization(thread, |r| {
            let selected = binding_id.and_then(|id| r.staging.memory_bindings.get(id).cloned());
            r.staging.memory = selected.clone();
            selected
        })?;
        match (binding_id, selected) {
            (_, Some(selected)) => Ok(Some(selected)),
            (None, None) => Ok(None),
            (Some(binding_id), None) => Err(RealizationError::invalid_request(format!(
                "automatic memory binding `{binding_id}` is not mounted for this Session"
            ))),
        }
    }
}

/// Spec projection and registration of the staged Resources.
impl ResourceStaging {
    /// Build the exact neutral Session spec for the provider selected by the
    /// immutable Agent publication. Network projection is provider capability
    /// dependent, so create/adopt/restore must not build with the process-wide
    /// default and then execute on a BackendOwned provider.
    pub fn sandbox_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let resources = self.thread_resources_snapshot(thread)?;
        self.sandbox_spec_for_resources_and_provider(thread, &resources, provider)
    }

    /// Build the provider-selected physical substrate with only mounts that the
    /// provider contract cannot attach later. The Session pending transition is
    /// already durable before this create-time Memory effect; after create, the
    /// V2 handle is persisted before any mutable workspace or Git effect.
    pub fn sandbox_substrate_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (create_time_mounts, has_repositories) = self
            .sessions
            .read_realization(thread, |r| {
                let create_time_mounts = r
                    .staging
                    .resources
                    .mounts
                    .iter()
                    .filter(|mount| resource_mount_is_create_time(mount))
                    .cloned()
                    .collect();
                let has_repositories = r.staging.transition.as_ref().is_some_and(|transition| {
                    transition.desired().resources.inputs().iter().any(|input| {
                        matches!(
                            input.source,
                            session_resource::ResolvedInputSource::Repository { .. }
                        )
                    })
                });
                (create_time_mounts, has_repositories)
            })?
            .unwrap_or_default();
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            create_time_mounts,
            has_repositories,
            provider.realizer().capabilities().network_isolation,
        )
    }

    pub fn sandbox_spec_for_resources_and_provider(
        &self,
        thread: &str,
        resources: &StagedResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            resources.mounts.clone(),
            !resources.repositories.is_empty(),
            provider.realizer().capabilities().network_isolation,
        )
    }

    /// Project a frozen aggregate manifest into the exact create-time provider
    /// substrate without loading any File, Memory, Skill, Vault, or Repository
    /// material. File mounts remain on the canonical late-attach path; cold
    /// adoption retains only typed Memory identity for handle validation.
    pub fn sandbox_spec_for_resolved_resources_and_provider(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let environment = self.sessions.frozen(thread)?.environment_projection.clone();
        let mounts = resources
            .inputs()
            .iter()
            .filter_map(|input| {
                crate::managed_input_layout::resolved_input_validation_mount(
                    input,
                    environment.as_ref(),
                )
            })
            .filter(resource_mount_is_create_time)
            .collect();
        let has_repositories = resources.inputs().iter().any(|input| {
            matches!(
                input.source,
                session_resource::ResolvedInputSource::Repository { .. }
            )
        });
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            mounts,
            has_repositories,
            provider.realizer().capabilities().network_isolation,
        )
    }

    pub fn sandbox_spec_for_mount_layout_with_network(
        &self,
        thread: &str,
        resource_mounts: Vec<pc::MountRequirement>,
        has_repositories: bool,
        network_isolation: bool,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (baseline, content_delivery, environment) = self
            .sessions
            .modify_session(thread, |slot| {
                (
                    slot.inputs.baseline.clone(),
                    slot.realization.staging.content_delivery,
                    slot.inputs.environment_projection.clone(),
                )
            })?
            .unwrap_or_default();
        self.sandbox_spec_for_projected_layout(
            thread,
            ProjectedSandboxLayout {
                resource_mounts,
                has_repositories,
                baseline_mounts: baseline.as_ref().map(|baseline| baseline.mounts.as_slice()),
                baseline_env: baseline.as_ref().map(|baseline| baseline.env.as_slice()),
                content_delivery,
                environment: environment.as_ref(),
                network_isolation,
            },
        )
    }

    pub fn sandbox_spec_for_projected_layout(
        &self,
        thread: &str,
        projection: ProjectedSandboxLayout<'_>,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let mounts = project_session_mounts(
            projection.resource_mounts,
            projection.baseline_mounts,
            projection.content_delivery,
        );
        let env = projection
            .baseline_env
            .map(<[pc::EnvVar]>::to_vec)
            .map_or_else(|| self.thread_session_env(thread), Ok)?;
        let spec = environment_projection::sandbox_spec_from_projection(
            thread,
            mounts,
            env,
            projection.environment,
            projection.network_isolation,
        );
        Ok(
            if projection.environment.is_some_and(|environment| {
                environment
                    .sandbox
                    .as_ref()
                    .is_some_and(|sandbox| sandbox.host_access == pc::HostAccessPolicy::TrustedHost)
            }) {
                // Preserve the frozen exact trust decision, just as placement does.
                // TrustedHost clients consume AgentFilesystemView, not an asserted
                // sandbox-absolute process namespace. Provider admission still checks
                // every requested capability against the exact Workdir provider.
                spec
            } else {
                pc::session_sandbox_projection(&spec, projection.has_repositories, false).0
            },
        )
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    /// REPLACES the Thread's set (correct at create time, before its first Run).
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        // A create-time fixture installs the Session it registers.
        self.sessions
            .install_session(thread)
            .and_then(|()| {
                self.sessions
                    .update_realization(thread, |r| r.staging.resources = staged)
            })
            .expect("test fixture registry is healthy");
    }

    pub fn register_thread_resource_manifest(
        &self,
        thread: &str,
        manifest: session_resource::SessionResourceManifest,
    ) -> Result<(), RealizationError> {
        self.sessions
            .update_realization(thread, |r| r.staging.manifest = Some(manifest))?;
        Ok(())
    }

    pub fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Result<Option<session_resource::SessionResourceManifest>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging.manifest.clone())?
            .flatten())
    }

    pub fn thread_resources_snapshot(
        &self,
        thread: &str,
    ) -> Result<StagedResources, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging.resources.clone())?
            .unwrap_or_default())
    }

    /// The Repository activations queued for `thread` (test-only observability: a
    /// working tree is realized through its port, not as a byte mount, so it is absent
    /// from `sandbox_spec`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn thread_repository_activations(
        &self,
        thread: &str,
    ) -> Vec<crate::staged_resources::RepositoryActivation> {
        self.sessions
            .read_realization(thread, |r| r.staging.resources.repositories.clone())
            .expect("test fixture registry is healthy")
            .unwrap_or_default()
    }

    pub fn thread_session_env(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_provisioning_contract::EnvVar>, RealizationError> {
        Ok(self
            .sessions
            .modify_session(thread, |slot| {
                slot.inputs
                    .baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })?
            .unwrap_or_default())
    }
}
