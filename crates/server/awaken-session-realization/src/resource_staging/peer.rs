// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Resource staging seen through its peer view; each method delegates to the existing owner method.
//! ResourceStaging as the view its peers read ([`crate::peers::StagingPeer`]).

use crate::realization_error::RealizationError;
use crate::staged_resources::{
    RepositoryActivation, RepositoryPublicationActivationError, ResourceBindingCheck,
    StagedResources,
};
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_types::RunError;
use std::sync::Arc;

use super::ResourceStaging;

#[async_trait::async_trait]
impl crate::peers::StagingPeer for ResourceStaging {
    fn fetches_resources_upstream(&self) -> bool {
        ResourceStaging::fetches_resources_upstream(self)
    }

    fn memory_reference_encoder(
        &self,
    ) -> Option<&Arc<dyn crate::memory_references::SessionMemoryReferenceEncoder>> {
        ResourceStaging::memory_reference_encoder(self)
    }

    fn sandbox_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        ResourceStaging::sandbox_spec_for_provider(self, thread, provider)
    }

    fn sandbox_spec_for_resolved_resources_and_provider(
        &self,
        thread: &str,
        resources: &session_resource::ResolvedSessionResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        ResourceStaging::sandbox_spec_for_resolved_resources_and_provider(
            self, thread, resources, provider,
        )
    }

    fn sandbox_substrate_spec_for_provider(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<pc::SandboxSpec, RealizationError> {
        ResourceStaging::sandbox_substrate_spec_for_provider(self, thread, provider)
    }

    fn thread_resource_manifest(
        &self,
        thread: &str,
    ) -> Result<Option<session_resource::SessionResourceManifest>, RealizationError> {
        ResourceStaging::thread_resource_manifest(self, thread)
    }

    fn thread_resources_snapshot(&self, thread: &str) -> Result<StagedResources, RealizationError> {
        ResourceStaging::thread_resources_snapshot(self, thread)
    }

    async fn apply_session_inputs_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        ResourceStaging::apply_session_inputs_under_lifecycle(
            self,
            lifecycle,
            peers,
            thread,
            transition,
            claim,
            realization_lease,
        )
        .await
    }

    async fn stage_terminal_repository_publication_input(
        &self,
        workspace: &str,
        command: &awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_types::RunError> {
        ResourceStaging::stage_terminal_repository_publication_input(
            self, workspace, command, lease,
        )
        .await
    }

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
            awaken_session_state_contract::SessionRepositoryPublicationCommand,
            session_resource::SessionRealizationLease,
        )>,
    ) -> Result<pc::RepositoryPublicationReceipt, RepositoryPublicationActivationError> {
        ResourceStaging::publish_repository_activation(
            self,
            thread,
            fallback_workspace,
            repository,
            binding_checks,
            realizer,
            expectation,
            publication_fence,
        )
        .await
    }
}
