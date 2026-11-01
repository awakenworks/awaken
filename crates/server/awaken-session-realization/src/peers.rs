// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Read-only views of the realization components; the authorization and binding-persistence
// @formal reason: operations they name keep their existing owners, fences and receipts.
//! What one realization component may read of its peers. The executor owns the
//! four components; for an operation that consults a peer it lends the
//! component it drives a [`Peers`] view. Each view is the narrow interface its
//! callers use (ADR-0091 D7, interface segregation), declared here so that no
//! component module depends on another: components depend on these views, and
//! only the executor knows all four.

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError,
    TerminalEnvironmentInstallation,
};
use crate::execution_authority::ExecutionAuthority;
use crate::realization_error::RealizationError;
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, BoundEnvironmentPreparationMode, PreparedBoundEnvironment,
    SandboxProviderRoles,
};
use crate::staged_resources::{
    RepositoryActivation, RepositoryPublicationActivationError, ResourceBindingCheck,
    StagedResources,
};
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_contract::SessionEnvironmentEffectIntent;
use awaken_session_types::RunError;
use std::sync::Arc;

/// Shared views of the realization components, lent by the executor.
#[derive(Clone, Copy)]
pub struct Peers<'a> {
    pub environments: &'a dyn EnvironmentPeer,
    pub staging: &'a dyn StagingPeer,
    pub effects: &'a dyn EffectsPeer,
    pub authority: &'a ExecutionAuthority,
}

/// What Resource staging and terminal effects read of Environment holding.
#[async_trait::async_trait]
pub trait EnvironmentPeer: Send + Sync {
    fn providers(&self) -> &crate::environment_providers::EnvironmentProviders;
    fn publication(&self) -> &crate::session_publication::SessionPublicationSelection;
    async fn install_bound_environment_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: Peers<'_>,
        installation: TerminalEnvironmentInstallation<'_>,
        resident: Option<Arc<crate::session_environment::SessionEnvironment>>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>, RealizationError>;
    #[allow(clippy::too_many_arguments)]
    async fn prepare_bound_environment_for_effect_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: Peers<'_>,
        thread: &str,
        binding: &str,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<PreparedBoundEnvironment, RealizationError>;
    fn session_environment_adoption_spec(
        &self,
        peers: Peers<'_>,
        thread: &str,
        provider: &crate::session_environment::SandboxProviderRoles,
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError>;
    async fn session_environment(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, RealizationError>;
    async fn authorize_environment_effect_before_io(
        &self,
        peers: Peers<'_>,
        thread: &str,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<AuthorizedSessionEnvironmentEffect, RealizationError>;
    async fn persist_authorized_environment_binding(
        &self,
        peers: Peers<'_>,
        thread: &str,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError>;
    fn projected_session_environment_provider(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<&SandboxProviderRoles, RealizationError>;
    async fn evict_session_for_rebuild(&self, thread: &str) -> Result<(), RealizationError>;
    fn session_environment_provider_for_thread(
        &self,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SandboxProviderRoles, RealizationError>;
}

/// What Environment holding and terminal effects read of Resource staging.
#[async_trait::async_trait]
pub trait StagingPeer: Send + Sync {
    fn fetches_resources_upstream(&self) -> bool;
    fn memory_reference_encoder(
        &self,
    ) -> Option<&Arc<dyn crate::memory_references::SessionMemoryReferenceEncoder>>;
    fn sandbox_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError>;
    fn sandbox_spec_for_resolved_resources_and_provider(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError>;
    fn sandbox_substrate_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<pc::SandboxSpec, RealizationError>;
    fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Result<Option<session_resource::SessionResourceManifest>, RealizationError>;
    fn thread_resources_snapshot(&self, thread: &str) -> Result<StagedResources, RealizationError>;
    async fn apply_session_inputs_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), RunError>;
    async fn stage_terminal_repository_publication_input(
        &self,
        workspace: &str,
        command: &awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_types::RunError>;
    #[allow(clippy::too_many_arguments)]
    async fn publish_repository_activation(
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
    ) -> Result<pc::RepositoryPublicationReceipt, RepositoryPublicationActivationError>;
}

/// What Environment holding reads of terminal effects.
pub trait EffectsPeer: Send + Sync {
    fn retained_preparation_owner(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<bool, RealizationError>;
}
