// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The construction-time Session provider refuses every realization, adoption, restore and
// @formal reason: terminal reconstruction until the process composition installs one, so the shared Host
// @formal reason: never chooses a sandbox implementation itself. This link is not a formal-proof claim.
//! The Host's Session environment provider before the process composition
//! selects one (`SessionExecutorBuilder::with_session_environment_provider`).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffect;
use awaken_session_environment_contract::{
    EnvironmentAdoption, EnvironmentAdoptionLayout, EnvironmentRealizationKind,
    EnvironmentRealizer, EnvironmentTerminalRealization, SandboxProviderRoles, SessionEnvironment,
};

/// The construction-time Session provider: the Workdir adapter rooted at
/// `sandbox_root` in this crate's own tests, otherwise one that refuses every
/// realization until the process composition installs its provider.
pub(crate) fn default_session_provider(
    sandbox_root: PathBuf,
) -> (SandboxProviderRoles, Option<Arc<dyn pc::SandboxCapacity>>) {
    #[cfg(test)]
    {
        (
            awaken_session_environment::SandboxEnvironmentProvider::workdir(sandbox_root)
                .into_provider_roles(),
            None,
        )
    }
    #[cfg(not(test))]
    {
        let _ = sandbox_root;
        let provider = Arc::new(UninstalledSandboxProvider);
        (SandboxProviderRoles::new(provider.clone()), Some(provider))
    }
}

/// Refuses every Session environment effect: the shared Host never chooses a
/// sandbox implementation itself. Consumer: a Host whose composition installed
/// no provider, such as a split Coordinator that only projects Sessions to its
/// Workers and observes none of their sources.
struct UninstalledSandboxProvider;

fn not_installed() -> pc::SandboxError {
    pc::SandboxError::not_ready(
        "no Session environment provider is installed on this Host; the process composition \
         selects one from its Deployment",
    )
}

#[async_trait]
impl pc::SandboxReadiness for UninstalledSandboxProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        // Nothing is enforced because nothing can be realized.
        awaken_session_environment_contract::workdir_capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Err(not_installed())
    }
}

#[async_trait]
impl EnvironmentRealizer for UninstalledSandboxProvider {
    fn supports_host_identity(&self) -> bool {
        false
    }

    fn effective_spec(&self, _spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        Err(not_installed())
    }

    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        Err(not_installed())
    }

    async fn create_effective_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _effect: SandboxEffect<'_>,
        _source_handle: Option<&pc::SandboxHandle>,
        _kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(not_installed())
    }
}

#[async_trait]
impl EnvironmentAdoption for UninstalledSandboxProvider {
    fn effective_adoption_layout(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
    ) -> Result<EnvironmentAdoptionLayout, pc::SandboxError> {
        Err(not_installed())
    }

    async fn observe_effective(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
        _retained: Option<&SessionEnvironment>,
        _basis: pc::SandboxObservationBasis<'_>,
        _purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        Err(not_installed())
    }

    fn validate_closed_observation(
        &self,
        _handle: &pc::SandboxHandle,
        _observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        Err(not_installed())
    }

    async fn adopt_effective_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
        _effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(not_installed())
    }

    async fn rebind_retained(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
        _retained: &SessionEnvironment,
        _effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(not_installed())
    }
}

#[async_trait]
impl EnvironmentTerminalRealization for UninstalledSandboxProvider {
    fn terminal_data_is_readable(&self) -> bool {
        // No environment exists whose data could be read.
        false
    }

    async fn prepare_terminal_effective_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&pc::SandboxEffectFence>,
        _terminal_effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        Err(not_installed())
    }

    async fn restore(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        _store: &dyn pc::SandboxCheckpointStore,
        _effect: pc::SandboxEffect<'_>,
    ) -> Result<pc::SandboxHandle, pc::SandboxError> {
        Err(not_installed())
    }

    async fn dispose_restored(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        _authorization: &pc::SandboxDisposalAuthorization,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        Err(not_installed())
    }
}

#[async_trait]
impl pc::SandboxCapacity for UninstalledSandboxProvider {
    async fn prewarm_to(
        &self,
        _spec: &pc::SandboxSpec,
        _target: usize,
    ) -> Result<usize, pc::SandboxError> {
        Err(not_installed())
    }

    async fn discard_shape(&self, _spec: &pc::SandboxSpec) {
        // No capacity was ever created.
    }

    fn ready_capacity(&self, _spec: &pc::SandboxSpec) -> usize {
        0
    }

    async fn shutdown_capacity(&self) {
        // No capacity was ever created.
    }
}

impl pc::SandboxInstallation for UninstalledSandboxProvider {
    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {
        // Nothing realizes a mount until a provider is installed; the Host
        // hands the mounter to that provider when it arrives.
    }

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {
        // Nothing materializes a secret until a provider is installed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_uninstalled_provider_refuses_readiness_without_claiming_checkpoint_support() {
        // C1 no provider is installed; C2 readiness is read through the existing
        // realizer handle. R1 C1+C2 -> the same typed NotReady refusal, original
        // conservative capabilities and no checkpoint format. These reads grant
        // no realization authority and cannot turn an absent adapter into Ready.
        let provider = SandboxProviderRoles::new(Arc::new(UninstalledSandboxProvider));
        assert_eq!(
            provider.readiness().probe_ready().await,
            Err(not_installed())
        );
        assert_eq!(
            provider.readiness().capabilities(),
            awaken_session_environment_contract::workdir_capabilities()
        );
        assert!(provider.readiness().checkpoint_formats().is_empty());
    }

    #[tokio::test]
    async fn an_uninstalled_provider_refuses_every_environment_effect() {
        // Causes: no provider was installed; the SAME unavailable adapter also
        // supplies the existing SandboxCapacity refusal, rather than a pool.
        // U1 realization, U2 adoption, U3 image preparation and U4 enabled
        // capacity warmup still refuse as not ready. U4 uses the existing
        // EnvironmentProviders owner and its selected unavailable capability;
        // the missing provider cannot become success. U5 capacity remains zero.
        let unavailable = Arc::new(UninstalledSandboxProvider);
        let provider = SandboxProviderRoles::new(unavailable.clone());
        let capacity: Arc<dyn pc::SandboxCapacity> = unavailable;
        let spec = crate::sandbox_layout::agent_run_sandbox_spec("uninstalled");
        let handle = pc::SandboxHandle::new("local", "none");
        let refused = |error: pc::SandboxError, row: &str| {
            assert!(
                error
                    .to_string()
                    .contains("no Session environment provider"),
                "{row}: {error}"
            );
        };
        refused(
            provider
                .realizer()
                .create_unattached(&spec)
                .await
                .err()
                .expect("U1"),
            "U1",
        );
        refused(
            provider
                .adoption()
                .adopt_unattached(&spec, &handle)
                .await
                .err()
                .expect("U2"),
            "U2",
        );
        refused(
            provider
                .realizer()
                .prewarm_images(&[])
                .await
                .expect_err("U3"),
            "U3",
        );
        refused(capacity.prewarm_to(&spec, 1).await.expect_err("U4"), "U4");
        let mut providers = crate::environment_providers::EnvironmentProviders::new(
            std::env::temp_dir().join("uninstalled-capacity"),
            provider.clone(),
            Some(capacity),
            crate::auxiliary_sandbox::default_auxiliary_sandboxes(),
            crate::TrustedHostPolicy::Refused,
            crate::WarmPool::new(1, 1),
        );
        providers.runs_local_dispatch_pool = true;
        refused(
            providers
                .prewarm_default_environment_capacity(1)
                .await
                .expect_err("U4"),
            "U4",
        );
        assert_eq!(
            providers.ready_environment_snapshot_capacity(&crate::host::eager_environment()),
            0,
            "U5"
        );
        assert!(
            !provider.realizer().supports_host_identity(),
            "U1 no host identity"
        );
    }
}
