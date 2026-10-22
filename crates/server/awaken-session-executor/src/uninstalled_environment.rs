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
    EnvironmentAdoption, EnvironmentAdoptionLayout, EnvironmentCapacity,
    EnvironmentProviderInstallation, EnvironmentRealizationKind, EnvironmentRealizer,
    EnvironmentTerminalRealization, SessionEnvironment, SessionEnvironmentProvider,
};

/// The construction-time Session provider: the Workdir adapter rooted at
/// `sandbox_root` in this crate's own tests, otherwise one that refuses every
/// realization until the process composition installs its provider.
pub(crate) fn default_session_provider(sandbox_root: PathBuf) -> SessionEnvironmentProvider {
    #[cfg(test)]
    {
        awaken_session_environment::SandboxEnvironmentProvider::workdir(sandbox_root)
            .into_session_provider()
    }
    #[cfg(not(test))]
    {
        let _ = sandbox_root;
        uninstalled_session_provider()
    }
}

/// Fresh unavailable effect roles, including when a test/scenario previously
/// supplied an implicit Workdir fallback. Read-only composition must not retain
/// that fallback's terminal, capacity or installation capabilities.
pub(crate) fn uninstalled_session_provider() -> SessionEnvironmentProvider {
    SessionEnvironmentProvider::new(Arc::new(UninstalledSessionEnvironmentProvider))
}

/// Refuses every Session environment effect: the shared Host never chooses a
/// sandbox implementation itself. Consumer: a Host whose composition installed
/// no provider, such as a split Coordinator that only projects Sessions to its
/// Workers.
struct UninstalledSessionEnvironmentProvider;

fn not_installed() -> pc::SandboxError {
    pc::SandboxError::not_ready(
        "no Session environment provider is installed on this Host; the process composition \
         selects one from its Deployment",
    )
}

#[async_trait]
impl EnvironmentRealizer for UninstalledSessionEnvironmentProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        // Nothing is enforced because nothing can be realized.
        awaken_session_environment_contract::workdir_capabilities()
    }

    fn supports_host_identity(&self) -> bool {
        false
    }

    fn effective_spec(&self, _spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
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
impl EnvironmentAdoption for UninstalledSessionEnvironmentProvider {
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
impl EnvironmentTerminalRealization for UninstalledSessionEnvironmentProvider {
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
impl EnvironmentCapacity for UninstalledSessionEnvironmentProvider {
    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        Err(not_installed())
    }

    async fn prewarm(
        &self,
        _spec: &pc::SandboxSpec,
        _target: usize,
    ) -> Result<usize, pc::SandboxError> {
        Err(not_installed())
    }

    async fn discard_capacity(&self, _spec: &pc::SandboxSpec) {
        // No capacity was ever created.
    }

    fn ready_capacity(&self, _spec: &pc::SandboxSpec) -> usize {
        0
    }

    async fn shutdown_capacity(&self) {
        // No capacity was ever created.
    }
}

impl EnvironmentProviderInstallation for UninstalledSessionEnvironmentProvider {
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
    async fn an_uninstalled_provider_refuses_every_environment_effect() {
        // Decision table: U1 realization, U2 adoption and U4 capacity warmup
        // through the uninstalled provider => refused as not ready, naming the
        // missing installation; U5 no capacity is reported.
        let provider =
            SessionEnvironmentProvider::new(Arc::new(UninstalledSessionEnvironmentProvider));
        let spec = crate::sandbox_projection::agent_run_sandbox_spec("uninstalled");
        let handle = pc::SandboxHandle::new("local", "none");
        let refused = |error: pc::SandboxError, row: &str| {
            assert!(
                error
                    .to_string()
                    .contains("no Session environment provider"),
                "{row}: {error}"
            );
        };
        refused(provider.create(&spec).await.err().expect("U1"), "U1");
        refused(
            provider.adopt(&spec, &handle).await.err().expect("U2"),
            "U2",
        );
        refused(
            provider.capacity().prewarm(&spec, 1).await.expect_err("U4"),
            "U4",
        );
        assert_eq!(provider.capacity().ready_capacity(&spec), 0, "U5");
        assert!(
            !provider.realizer().supports_host_identity(),
            "U1 no host identity"
        );
    }
}
