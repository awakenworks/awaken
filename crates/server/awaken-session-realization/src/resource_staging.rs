// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Resource staging owns the original projection mutex and cancellation/cache fields under the canonical installed Session lock; publication still checks the existing realization proof.
//! Resource staging of the realization executor: the frozen Session Resources
//! as the Environment receives them. It projects the provider-effective sandbox
//! spec, registers the staged Resources and the active manifest, and validates
//! the managed mount layout against a provider.

use crate::realization_error::RealizationError;
use crate::sandbox_layout::{
    ProjectedSandboxLayout, create_time_resource_mounts, project_sandbox_spec,
    resource_mount_is_create_time,
};
use crate::staged_resources::StagedResources;
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;
use awaken_session_run_delivery::MemoryBinding;
use std::sync::Arc;

mod layout_validation;
mod peer;
pub mod preparation;
pub mod projection;
pub mod repository_activation;
mod requirement_compilation;
pub use preparation::ResourcePreparation;

/// The Resource sources and verifiers staging reads through, selected once by
/// the process composition. Its exact Skill byte source is selected before
/// construction, without catalog writes.
pub struct ResourceSources {
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
    /// The same exact materializer selected for the process, used only at a Git effect.
    pub credential_materializer:
        Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    pub memory_reference_encoder:
        Option<Arc<dyn crate::memory_references::SessionMemoryReferenceEncoder>>,
}

/// Stages one installed Session's Resources into its Environment spec; its only
/// state is the Resource staging state of the installed Sessions.
#[derive(Clone)]
pub struct ResourceStaging {
    sessions: crate::session_realizations::SessionRealizations,
    bundle_source: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
    file_content_source:
        Arc<dyn awaken_resource_contract::FileContentSource<ResourceOperationFence>>,
    memory: awaken_resource_contract::MemoryExecutionContent,
    resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    repository_binding_verifier: Option<
        Arc<dyn awaken_resource_contract::RepositoryBindingVerifier<ResourceOperationFence>>,
    >,
    fetches_resources_upstream: bool,
    credential_materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    memory_reference_encoder:
        Option<Arc<dyn crate::memory_references::SessionMemoryReferenceEncoder>>,
}

impl ResourceStaging {
    /// Original Memory-only roles, without retaining the Session staging owner.
    ///
    /// ```no_run
    /// use awaken_session_realization::resource_staging::ResourceStaging;
    /// use awaken_session_configuration_contract::resource::{ResolvedInput, SessionResourceManifest};
    ///
    /// async fn selected_memory(
    ///     staging: &ResourceStaging,
    ///     input: &ResolvedInput,
    ///     manifest: &SessionResourceManifest,
    /// ) -> Result<(), awaken_session_types::RunError> {
    ///     // R1: either resolved or frozen input uses the same selected compiler;
    ///     // both operations remain available without retaining the staging owner.
    ///     let compiler = staging.memory_binding_compiler();
    ///     compiler.compile("session", "workspace", input, None).await?;
    ///     compiler.compile_frozen("session", manifest, "memory").await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ```compile_fail,E0599
    /// use awaken_session_realization::resource_staging::ResourceStaging;
    /// use awaken_session_configuration_contract::resource::ResolvedInput;
    /// async fn retired_resolved(staging: &ResourceStaging, input: &ResolvedInput) {
    ///     // R2: a resolved input cannot use the retired forwarding entry point.
    ///     staging.compile_memory_binding("session", "workspace", input, None).await;
    /// }
    /// ```
    ///
    /// ```compile_fail,E0599
    /// use awaken_session_realization::resource_staging::ResourceStaging;
    /// use awaken_session_configuration_contract::resource::SessionResourceManifest;
    /// async fn retired_frozen(staging: &ResourceStaging, manifest: &SessionResourceManifest) {
    ///     // R3 is independent of R2: frozen recovery cannot use its old forwarder.
    ///     staging.compile_frozen_memory_binding("session", manifest, "memory").await;
    /// }
    /// ```
    pub fn memory_binding_compiler(&self) -> awaken_session_run_delivery::MemoryBindingCompiler {
        awaken_session_run_delivery::MemoryBindingCompiler::new(
            self.memory.clone(),
            self.resource_validator.clone(),
        )
    }
    /// Stage Resources of the installed Sessions through the selected sources.
    pub fn new(
        sessions: crate::session_realizations::SessionRealizations,
        sources: ResourceSources,
    ) -> Self {
        let ResourceSources {
            bundle_source,
            file_content_source,
            memory,
            resource_validator,
            repository_binding_verifier,
            fetches_resources_upstream,
            credential_materializer,
            memory_reference_encoder,
        } = sources;
        Self {
            sessions,
            bundle_source,
            file_content_source,
            memory,
            resource_validator,
            repository_binding_verifier,
            fetches_resources_upstream,
            credential_materializer,
            memory_reference_encoder,
        }
    }

    /// The installed Sessions this staging records into.
    pub fn sessions(&self) -> &crate::session_realizations::SessionRealizations {
        &self.sessions
    }

    /// The repository binding verifier of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn repository_binding_verifier(
        &self,
    ) -> Option<&Arc<dyn awaken_resource_contract::RepositoryBindingVerifier<ResourceOperationFence>>>
    {
        self.repository_binding_verifier.as_ref()
    }

    /// Whether Resource bytes are fetched upstream rather than read locally.
    pub fn fetches_resources_upstream(&self) -> bool {
        self.fetches_resources_upstream
    }

    /// The Memory reference encoder Memory effects publish through.
    pub fn memory_reference_encoder(
        &self,
    ) -> Option<&Arc<dyn crate::memory_references::SessionMemoryReferenceEncoder>> {
        self.memory_reference_encoder.as_ref()
    }

    pub fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_provisioning_contract::MountRequirement>, RealizationError> {
        Ok(self
            .sessions
            .modify_session(thread, |installed| {
                crate::sandbox_layout::project_session_mounts(
                    installed.realization.staging().resources.mounts.clone(),
                    installed
                        .inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice()),
                    installed.realization.staging().content_delivery,
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
            r.staging_mut().memory_bindings.clear();
            r.staging_mut().memory = memory;
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
            let selected = binding_id.and_then(|id| r.staging().memory_bindings.get(id).cloned());
            r.staging_mut().memory = selected.clone();
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
        provider: &crate::session_environment::SandboxProviderRoles,
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
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (create_time_mounts, has_repositories) = self
            .sessions
            .read_realization(thread, |r| {
                let create_time_mounts = r
                    .staging()
                    .resources
                    .mounts
                    .iter()
                    .filter(|mount| resource_mount_is_create_time(mount))
                    .cloned()
                    .collect();
                let has_repositories = r.staging().transition.as_ref().is_some_and(|transition| {
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
            provider.readiness().capabilities().network_isolation,
        )
    }

    pub fn sandbox_spec_for_resources_and_provider(
        &self,
        thread: &str,
        resources: &StagedResources,
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            resources.mounts.clone(),
            !resources.repositories.is_empty(),
            provider.readiness().capabilities().network_isolation,
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
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let environment = self.sessions.frozen(thread)?.environment_projection.clone();
        let mounts = create_time_resource_mounts(resources, environment.as_ref());
        let has_repositories = resources.compatibility_facts().has_repository();
        self.sandbox_spec_for_mount_layout_with_network(
            thread,
            mounts,
            has_repositories,
            provider.readiness().capabilities().network_isolation,
        )
    }

    pub(crate) fn sandbox_spec_for_mount_layout_with_network(
        &self,
        thread: &str,
        resource_mounts: Vec<pc::MountRequirement>,
        has_repositories: bool,
        network_isolation: bool,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        let (baseline, content_delivery, environment) = self
            .sessions
            .modify_session(thread, |installed| {
                (
                    installed.inputs.baseline.clone(),
                    installed.realization.staging().content_delivery,
                    installed.inputs.environment_projection.clone(),
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
        let env = projection
            .baseline_env
            .map(<[pc::EnvVar]>::to_vec)
            .map_or_else(|| self.thread_session_env(thread), Ok)?;
        Ok(project_sandbox_spec(thread, projection, env))
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
                    .update_realization(thread, |r| r.staging_mut().resources = staged)
            })
            .expect("test fixture registry is healthy");
    }

    pub fn register_thread_resource_manifest(
        &self,
        thread: &str,
        manifest: session_resource::SessionResourceManifest,
    ) -> Result<(), RealizationError> {
        self.sessions
            .update_realization(thread, |r| r.staging_mut().manifest = Some(manifest))?;
        Ok(())
    }

    pub fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Result<Option<session_resource::SessionResourceManifest>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging().manifest.clone())?
            .flatten())
    }

    pub fn thread_resources_snapshot(
        &self,
        thread: &str,
    ) -> Result<StagedResources, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.staging().resources.clone())?
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
            .read_realization(thread, |r| r.staging().resources.repositories.clone())
            .expect("test fixture registry is healthy")
            .unwrap_or_default()
    }

    pub fn thread_session_env(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_provisioning_contract::EnvVar>, RealizationError> {
        Ok(self
            .sessions
            .modify_session(thread, |installed| {
                installed
                    .inputs
                    .baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })?
            .unwrap_or_default())
    }
}
