// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Selection of the Session environment providers the Session lifecycle realizes through;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Environment dependencies of a Host: the Session-owned environment
//! provider, the trusted-host BackendOwned provider, deliberately fresh
//! housekeeping sandboxes, checkpoint custody, CacheVolume preparation, the
//! credential-file broker, and the Memory mount adapter every provider shares.

use crate::host::*;

impl HostDependencies {
    /// Select the sole checkpoint-byte adapter. This does not install lifecycle
    /// state; Session root CAS remains authoritative.
    #[must_use]
    pub fn with_environment_checkpoint_store(
        mut self,
        store: Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>,
    ) -> Self {
        self.env.environment_checkpoint_store = Some(store);
        self
    }

    /// Install the credential-file broker shared by every Session sandbox tier
    /// on the providers selected so far. Application plans retain opaque
    /// `MountSource::Secret` references; only the selected provider asks this
    /// port for bytes at realization time.
    #[must_use]
    pub fn with_session_secret_broker(
        self,
        broker: Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) -> Self {
        self.env
            .session_provider
            .installation()
            .install_secret_broker(broker.clone());
        if let Some(provider) = &self.env.backend_owned_session_provider {
            provider.installation().install_secret_broker(broker);
        }
        self
    }

    /// Select the authoritative provider of every Session environment on this
    /// Host. The process composition selects and builds it from the Deployment
    /// (`awaken-execution-composition`); the Host continues to own Session
    /// routing and lifecycle. A caller that also selects a
    /// [`awaken_provisioning_contract::SecretBroker`] does so afterwards, so the
    /// broker lands on this authoritative provider.
    #[must_use]
    pub fn with_session_environment_provider(
        mut self,
        provider: awaken_session_environment_contract::SessionEnvironmentProvider,
    ) -> Self {
        self.env.session_provider = provider;
        self.env.session_provider_explicit = true;
        if let Some(mounter) = self.env.memory_mounter.clone() {
            self.env
                .session_provider
                .installation()
                .install_memory_mounter(mounter);
        }
        self
    }

    /// Select a non-authoritative fallback provider: it serves Sessions unless
    /// Deployment selection selects the authoritative one, which replaces it.
    /// Consumers: test and Scenario compositions that deliberately keep the
    /// cheap Workdir adapter.
    #[must_use]
    pub fn with_fallback_session_environment_provider(
        mut self,
        provider: awaken_session_environment_contract::SessionEnvironmentProvider,
    ) -> Self {
        self.env.session_provider = provider;
        self.env.session_provider_explicit = false;
        if let Some(mounter) = self.env.memory_mounter.clone() {
            self.env
                .session_provider
                .installation()
                .install_memory_mounter(mounter);
        }
        self
    }

    /// Select the TrustedHost provider that serves BackendOwned provisioning
    /// and `trusted_host` Environments. Without one, both fail closed.
    #[must_use]
    pub fn with_backend_owned_environment_provider(
        mut self,
        provider: awaken_session_environment_contract::SessionEnvironmentProvider,
    ) -> Self {
        if let Some(mounter) = self.env.memory_mounter.clone() {
            provider.installation().install_memory_mounter(mounter);
        }
        self.env.backend_owned_session_provider = Some(provider);
        self
    }

    /// Whether the composition already selected the authoritative Session
    /// provider. The container-tier composition selects its provider only
    /// when none is; deployment selection likewise preserves a selected one.
    #[must_use]
    pub fn session_environment_provider_installed(&self) -> bool {
        self.env.session_provider_explicit
    }

    /// The selected Session environment provider, for the composition that
    /// selected it to verify its startup.
    #[must_use]
    pub fn installed_session_environment_provider(
        &self,
    ) -> &awaken_session_environment_contract::SessionEnvironmentProvider {
        &self.env.session_provider
    }

    /// The selected TrustedHost provider, if any, for the composition that
    /// selected it to verify its startup.
    #[must_use]
    pub fn installed_backend_owned_environment_provider(
        &self,
    ) -> Option<&awaken_session_environment_contract::SessionEnvironmentProvider> {
        self.env.backend_owned_session_provider.as_ref()
    }

    /// Container-provider selection for this crate's own tests: the
    /// canonical container environment over `provider` with these
    /// dependencies' Deployment Hand settings.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_session_container_provider(
        self,
        provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
        hand_factory: Arc<dyn awaken_session_environment_contract::HandExecutorFactory>,
    ) -> Self {
        let hand_bin = self.deployment.sandbox.container_hand_bin.clone();
        let environment = crate::session_environment::SandboxEnvironmentProvider::
            container_with_capacity_hand_idle_and_residency(
                provider,
                None,
                Vec::new(),
                hand_factory,
                hand_bin,
                std::time::Duration::from_secs(self.deployment.sandbox.container_hand_idle_secs),
                self.deployment.sandbox.container_hand_residency,
                self.clock.clone(),
            )
            .into_session_provider();
        self.with_session_environment_provider(environment)
    }

    /// Select the process composition's auxiliary-run sandbox factory. Every
    /// auxiliary Agent Run (compactor, Memory selection/extraction, isolated
    /// Skill fork) realizes its fresh sandbox through it; a Host without one
    /// refuses those Runs instead of choosing a sandbox implementation itself.
    #[must_use]
    pub fn with_auxiliary_sandboxes(
        mut self,
        factory: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    ) -> Self {
        self.env.auxiliary_sandboxes = factory;
        self
    }

    /// Replace the default directory-only CacheVolume preparation with one
    /// product-specific initializer. Explicit warmup and Session creation keep
    /// sharing the same single-flight owner.
    #[must_use]
    pub fn with_cache_volume_initializer(
        mut self,
        initializer: Arc<dyn crate::CacheVolumeInitializer>,
    ) -> Self {
        self.env.cache_volume_prewarmer =
            crate::cache_volume::CacheVolumePrewarmer::new(initializer);
        self
    }

    /// Select the worker adapter behind the neutral provisioning port and
    /// install it on every provider selected so far; a provider selected later
    /// receives it too. Runtime-host never depends on a FUSE/copy
    /// implementation crate.
    #[must_use]
    pub fn with_memory_mounter(
        mut self,
        mounter: Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) -> Self {
        self.env
            .session_provider
            .installation()
            .install_memory_mounter(mounter.clone());
        if let Some(provider) = &self.env.backend_owned_session_provider {
            provider
                .installation()
                .install_memory_mounter(mounter.clone());
        }
        self.env.memory_mounter = Some(mounter);
        self
    }

    /// Whether the composition already selected the Memory mount adapter.
    /// Default platform wiring must preserve an explicit selection.
    #[must_use]
    pub fn has_memory_mounter(&self) -> bool {
        self.env.memory_mounter.is_some()
    }
}
