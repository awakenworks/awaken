// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Environment holding seen through its peer view; each method delegates to the existing owner method.
//! EnvironmentHolding as the view its peers read ([`crate::peers::EnvironmentPeer`]).

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError,
    TerminalEnvironmentInstallation,
};
use crate::realization_error::RealizationError;
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, BoundEnvironmentPreparationMode, PreparedBoundEnvironment,
    SessionEnvironmentProvider,
};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_state_contract::SessionEnvironmentEffectIntent;
use std::sync::Arc;

use super::EnvironmentHolding;

#[async_trait::async_trait]
impl crate::peers::EnvironmentPeer for EnvironmentHolding {
    fn providers(&self) -> &crate::environment_providers::EnvironmentProviders {
        &self.providers
    }

    fn publication(&self) -> &crate::session_publication::SessionPublicationSelection {
        &self.publication
    }

    async fn install_bound_environment_under_lifecycle(
        &self,
        peers: crate::peers::Peers<'_>,
        installation: TerminalEnvironmentInstallation<'_>,
        resident: Option<Arc<crate::session_environment::SessionEnvironment>>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>, RealizationError>
    {
        EnvironmentHolding::install_bound_environment_under_lifecycle(
            self,
            peers,
            installation,
            resident,
            mode,
        )
        .await
    }

    async fn prepare_bound_environment_for_effect_under_lifecycle(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        binding: &str,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<PreparedBoundEnvironment, RealizationError> {
        EnvironmentHolding::prepare_bound_environment_for_effect_under_lifecycle(
            self,
            peers,
            thread,
            binding,
            effect_fence,
            resolved_resources,
            mode,
        )
        .await
    }

    fn session_environment_adoption_spec(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError> {
        EnvironmentHolding::session_environment_adoption_spec(
            self,
            peers,
            thread,
            provider,
            resolved_resources,
        )
    }

    async fn session_environment(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, RealizationError> {
        EnvironmentHolding::session_environment(self, thread).await
    }

    async fn authorize_environment_effect_before_io(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<AuthorizedSessionEnvironmentEffect, RealizationError> {
        EnvironmentHolding::authorize_environment_effect_before_io(self, peers, thread, intent)
            .await
    }

    async fn persist_authorized_environment_binding(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError> {
        EnvironmentHolding::persist_authorized_environment_binding(
            self, peers, thread, binding, effect,
        )
        .await
    }

    fn projected_session_environment_provider(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<&SessionEnvironmentProvider, RealizationError> {
        EnvironmentHolding::projected_session_environment_provider(self, thread, agent)
    }

    async fn evict_session_for_rebuild(&self, thread: &str) -> Result<(), RealizationError> {
        EnvironmentHolding::evict_session_for_rebuild(self, thread).await
    }

    fn session_environment_provider_for_thread(
        &self,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, RealizationError> {
        EnvironmentHolding::session_environment_provider_for_thread(self, thread, provisioning)
    }
}
