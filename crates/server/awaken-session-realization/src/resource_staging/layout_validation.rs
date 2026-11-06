// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Managed mount layout validation against the Session provider before any Environment effect;
// @formal reason: it authorizes nothing itself and keeps the existing projection and provider checks.
//! Layout validation of Resource staging: the managed mount layout a Resource
//! set, a binding or a prospective Sandbox layout projects, validated against
//! the Session's provider before any Environment effect.
use super::ResourceStaging;
use crate::realization_error::RealizationError;
use awaken_session_configuration_contract::environment_projection;
use awaken_session_configuration_contract::resource as session_resource;

use crate::managed_input_layout::ManagedLayoutValidationScope;
use crate::managed_layout::{
    ManagedLayoutProjection, binding_layout_projection, resolved_input_layout_projection,
};
use crate::sandbox_layout::ProjectedSandboxLayout;

impl ResourceStaging {
    fn validate_managed_layout_inputs(
        &self,
        thread: &str,
        inputs: impl IntoIterator<Item = ManagedLayoutProjection>,
        scope: ManagedLayoutValidationScope<'_>,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<(), RealizationError> {
        let (resident_baseline, content_delivery, resident_environment, live_environment) = self
            .sessions
            .modify_session(thread, |installed| {
                (
                    installed.inputs.baseline.clone(),
                    installed.realization.staging().content_delivery,
                    installed.inputs.environment_projection.clone(),
                    installed.realization.environment().owner.resident(),
                )
            })?
            .unwrap_or_default();
        let environment = environment.or(resident_environment.as_ref());
        let mut mounts = Vec::new();
        let mut repository_paths = Vec::new();
        for input in inputs {
            match input {
                ManagedLayoutProjection::Mount(mount) => {
                    mounts.push(mount.validation_requirement(environment));
                }
                ManagedLayoutProjection::Repository { mount_path, .. } => {
                    repository_paths.push(mount_path)
                }
            }
        }
        let spec = self.sandbox_spec_for_projected_layout(
            thread,
            ProjectedSandboxLayout {
                resource_mounts: mounts,
                has_repositories: !repository_paths.is_empty(),
                baseline_mounts: baseline_mounts.or_else(|| {
                    resident_baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice())
                }),
                baseline_env: baseline_env.or_else(|| {
                    resident_baseline
                        .as_ref()
                        .map(|baseline| baseline.env.as_slice())
                }),
                content_delivery,
                environment,
                network_isolation: scope.network_isolation(),
            },
        )?;
        let historical_owned_paths = live_environment.as_ref().and_then(|environment| {
            environment
                .lifecycle()
                .handle()
                .owned_paths()
                .map(<[String]>::to_vec)
        });
        if live_environment.is_some()
            && !repository_paths.is_empty()
            && historical_owned_paths.is_none()
        {
            return Err(RealizationError::invariant(
                "legacy Session sandbox handle lacks complete owned-path evidence required for live Repository replacement",
            ));
        }
        let historical_owned_paths = historical_owned_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let repository_paths = repository_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        match scope {
            ManagedLayoutValidationScope::Structural => {
                awaken_provisioning_contract::validate_repository_sandbox_adoption_layout(
                    &repository_paths,
                    &spec,
                    &historical_owned_paths,
                )
                .map_err(RealizationError::from)
            }
            ManagedLayoutValidationScope::Exact(provider) => self
                .validate_repository_environment_adoption_paths(
                    provider,
                    &spec,
                    &repository_paths,
                    &historical_owned_paths,
                )
                .map(|_| ()),
        }
    }

    pub fn validate_resource_layout(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        provider: &crate::session_environment::SandboxProviderRoles,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<(), RealizationError> {
        self.validate_managed_layout_inputs(
            thread,
            resources
                .inputs()
                .iter()
                .map(resolved_input_layout_projection),
            ManagedLayoutValidationScope::Exact(provider),
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub fn validate_structural_resource_layout(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<(), RealizationError> {
        self.validate_managed_layout_inputs(
            thread,
            resources
                .inputs()
                .iter()
                .map(resolved_input_layout_projection),
            ManagedLayoutValidationScope::Structural,
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub fn validate_binding_layout(
        &self,
        thread: &str,
        resources: &[awaken_resource_contract::InputBinding],
        provider: &crate::session_environment::SandboxProviderRoles,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<(), RealizationError> {
        self.validate_managed_layout_inputs(
            thread,
            resources.iter().map(binding_layout_projection),
            ManagedLayoutValidationScope::Exact(provider),
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub fn validate_structural_binding_layout(
        &self,
        thread: &str,
        resources: &[awaken_resource_contract::InputBinding],
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
    ) -> Result<(), RealizationError> {
        self.validate_managed_layout_inputs(
            thread,
            resources.iter().map(binding_layout_projection),
            ManagedLayoutValidationScope::Structural,
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub fn validate_prospective_session_layout(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        let environments = peers.environments;
        let staging = self;
        let environment = environment_projection::project_environment(&layout.environment);
        let (_, candidate) = environments
            .publication()
            .resolve_canonical_session_projection(
                &layout.workspace_id,
                awaken_session_execution_contract::CanonicalSessionProjection::Layout(layout),
                None,
                None,
            )
            .map_err(|error| {
                awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
            })?;
        let result =
            if layout.runtime_placement == awaken_session_types::SessionRuntimePlacement::Worker {
                staging.validate_structural_binding_layout(
                    thread,
                    &layout.resources,
                    Some(&layout.mounts),
                    Some(&layout.env),
                    Some(&environment),
                )
            } else {
                let provider = if let Some(candidate) = candidate.as_ref() {
                    environments
                        .session_environment_provider_for_thread(thread, candidate.provisioning())
                } else {
                    environments.session_environment_provider_for_thread(
                        thread,
                        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                    )
                }
                .map_err(|error| {
                    awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
                })?;
                staging.validate_binding_layout(
                    thread,
                    &layout.resources,
                    provider,
                    Some(&layout.mounts),
                    Some(&layout.env),
                    Some(&environment),
                )
            };
        result.map_err(|error| {
            awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
        })
    }

    pub fn validate_effective_resource_layout(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        let environments = peers.environments;
        let staging = self;
        let provider = environments
            .projected_session_environment_provider(thread, None)
            .map_err(|error| {
                awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
            })?;
        staging
            .validate_resource_layout(thread, resources, provider, None, None, None)
            .map_err(|error| {
                awaken_runtime_contract::runs::RunError::bad_request(error.to_string())
            })
    }
}
