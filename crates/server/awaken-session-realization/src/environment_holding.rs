// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Environment component owns the original per-Session lifecycle mutex and physical owner projection; no second registry or durable authority is introduced.
//! Environment holding of the realization executor: the Environment each
//! installed Session holds, through the sandbox providers it realizes with.
//! It selects the provider, creates, adopts, restores and retires the exact
//! Environment owner under the Session's lifecycle lock, and keeps the warm
//! capacity those providers prepare.

pub mod bound_environment;
pub mod lifecycle;
mod peer;
pub mod realization;

/// Holds each installed Session's Environment through the sandbox providers it
/// owns. Resource staging and the execution authority are peers: an
/// operation that consults them receives them from the executor.
pub struct EnvironmentHolding {
    /// The selected aggregate publication authority; the lock permits fixture installation only.
    environment_binding_sink: std::sync::RwLock<
        Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// The same dispatch instance used by this process, narrowed to execution binding.
    dispatch_execution: Option<std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>>,
    /// Frozen publication selection over the installed Sessions.
    publication: crate::session_publication::SessionPublicationSelection,
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    providers: crate::environment_providers::EnvironmentProviders,
    /// The installed Sessions, as far as realization may reach them.
    sessions: crate::session_realizations::SessionRealizations,
}

impl EnvironmentHolding {
    /// Hold Environments over the installed Sessions with the sandbox providers
    /// they realize through.
    pub fn new(
        mut providers: crate::environment_providers::EnvironmentProviders,
        runs_local_dispatch_pool: bool,
        sessions: crate::session_realizations::SessionRealizations,
        environment_binding_sink: Option<
            std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
        >,
        dispatch_execution: Option<
            std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>,
        >,
        publication: crate::session_publication::SessionPublicationSelection,
    ) -> Self {
        providers.runs_local_dispatch_pool = runs_local_dispatch_pool;
        Self {
            environment_binding_sink: std::sync::RwLock::new(environment_binding_sink),
            dispatch_execution,
            publication,
            providers,
            sessions,
        }
    }

    /// The binding sink of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn environment_binding_sink(
        &self,
    ) -> Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>
    {
        self.environment_binding_sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the dispatch execution role of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_dispatch_execution(
        &mut self,
        dispatch_execution: Option<
            std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>,
        >,
    ) {
        self.dispatch_execution = dispatch_execution;
    }

    /// Replace the binding sink of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        *self
            .environment_binding_sink
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }
}

use crate::realization_error::RealizationError;
use crate::sandbox_projection::{
    ProjectedSandboxLayout, create_time_resource_mounts, project_sandbox_spec,
};
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::{environment_projection, resource as session_resource};
use awaken_session_realization_contract::{CanonicalSessionProjection, FrozenSessionProjection};

impl EnvironmentHolding {
    /// Read original source retention without installing or changing a Session.
    pub async fn observe_terminal_cleanup_retention(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, RealizationError> {
        let Some(binding) = projection.environment.binding() else {
            return Ok(false);
        };
        let resources = projection
            .previous_resource_manifest
            .as_ref()
            .map_or(&projection.resources, |manifest| &manifest.resources);
        let has_memory = |resources: &session_resource::ResolvedSessionResources| {
            resources.inputs().iter().any(|input| {
                matches!(
                    input.source,
                    session_resource::ResolvedInputSource::MemoryStore { .. }
                )
            })
        };
        if !source_prepared && (has_memory(resources) || has_memory(&projection.resources)) {
            return Ok(false);
        }
        let (_, candidate) = self.publication.resolve_canonical_session_projection(
            &projection.workspace_id,
            CanonicalSessionProjection::Baseline(&projection.baseline),
            projection.agent_publication.clone(),
            Some(projection.resources.skills()),
        )?;
        let provisioning = candidate.as_ref().map_or(
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            |candidate| candidate.provisioning(),
        );
        let environment =
            environment_projection::project_environment(&projection.baseline.environment);
        let provider = realization::session_environment_provider_for_environment(
            &self.providers,
            Some(&environment),
            provisioning,
        )?;
        let spec = project_sandbox_spec(
            session_id.as_str(),
            ProjectedSandboxLayout {
                resource_mounts: create_time_resource_mounts(resources, Some(&environment)),
                has_repositories: resources.compatibility_facts().has_repository(),
                baseline_mounts: Some(&projection.baseline.mounts),
                baseline_env: Some(&projection.baseline.env),
                content_delivery: None,
                environment: Some(&environment),
                network_isolation: provider.realizer().capabilities().network_isolation,
            },
            projection.baseline.env.clone(),
        );
        let spec = provider
            .realizer()
            .effective_spec(&spec)
            .map_err(RealizationError::from)?;
        if !source_prepared
            && spec
                .mounts
                .iter()
                .any(|mount| matches!(mount.source, pc::MountSource::MemoryStore { .. }))
        {
            return Ok(false);
        }
        let handle = lifecycle::decode_session_environment_binding(session_id.as_str(), binding)?;
        let original = if projection.environment_materialization.is_some() {
            Some(
                projection
                    .original_create_fence(session_id.as_str())
                    .map_err(RealizationError::from)?,
            )
        } else {
            None
        };
        let basis =
            original
                .as_ref()
                .map_or(pc::SandboxObservationBasis::Unattached, |effect_fence| {
                    pc::SandboxObservationBasis::Effect {
                        spec: &spec,
                        effect_fence,
                    }
                });
        let observed = provider
            .adoption()
            .observe_effective(
                &spec,
                &handle,
                None,
                basis,
                pc::SandboxObservationPurpose::WorkerRetirement { source_prepared },
            )
            .await
            .map_err(RealizationError::from)?;
        Ok(match observed {
            pc::SandboxObservation::Incompatible { .. } => false,
            pc::SandboxObservation::RetainedOriginal { .. } => true,
            _ => source_prepared,
        })
    }
}
