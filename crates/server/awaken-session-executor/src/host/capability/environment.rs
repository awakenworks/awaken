// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Installers of the Session environment providers the Session lifecycle realizes through;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! ENV capability of [`SharedHost`]: the Session-owned environment provider,
//! the trusted-host BackendOwned provider, deliberately fresh housekeeping
//! sandboxes, checkpoint custody, CacheVolume preparation, and the Memory
//! mount adapter every provider shares.

use crate::host::*;

/// The one container-environment realization assembled from a deployment by
/// the worker-plane composition (`awaken-execution-composition`).
///
/// A downstream composition may decorate `provider` while retaining the exact
/// capacity and cache-volume owners created by the canonical builder. This
/// prevents product integrations from reproducing Kubernetes/Podman/Docker
/// construction merely to add an infrastructure concern such as checkpointing.
/// Consumer: [`SharedHost::with_session_container_environment_components`].
pub struct ContainerEnvironmentComponents {
    pub provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
    pub capacity: Option<Arc<dyn awaken_container_contract::ContainerEnvironmentCapacity>>,
    pub extra_mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub cache_volume_initializer: Option<Arc<dyn crate::CacheVolumeInitializer>>,
}

/// Sandbox-provider wiring. Consumer: Session environment realization and
/// lifecycle, ACP backend selection, warm capacity, and helper sub-agents.
pub(crate) struct EnvironmentProviders {
    /// Deliberately fresh sandboxes for auxiliary Agent Runs (compactor, Memory
    /// selection/extraction, isolated Skill forks), injected by the process
    /// composition. They never share the Session-owned environment.
    pub(crate) auxiliary_sandboxes:
        Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    /// Provider for the Session-owned environment shared by Native/ACP/children.
    /// Kept separate from deliberately-fresh housekeeping sandboxes.
    pub(crate) session_provider: crate::session_environment::SessionEnvironmentProvider,
    /// Deployment-owned byte custody for full-Environment checkpoints. Desired
    /// lifecycle state remains in the Session aggregate; this port stores bytes
    /// only. Hosted startup injects the regional encrypted adapter.
    pub(crate) environment_checkpoint_store:
        Option<Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>>,
    /// Product-plane single-flight preparation for caller-owned CacheVolume
    /// paths. The selected Sandbox provider remains an opaque-path consumer.
    pub(crate) cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer,
    /// Trusted-host environment selected only for BackendOwned provisioning.
    /// It is a policy branch over the same Session owner, not a second executor.
    pub(crate) backend_owned_session_provider:
        Option<crate::session_environment::SessionEnvironmentProvider>,
    /// The process startup installed the authoritative Session provider. ACP
    /// ACP execution must reuse it instead of constructing a deployment-derived peer.
    pub(crate) session_provider_explicit: bool,
    /// Worker-side service for governed MemoryStore mounts. The Runtime Host
    /// stores only the neutral contract; the outer server process installs
    /// the FUSE/copy adapter.
    pub(crate) memory_mounter:
        std::sync::RwLock<Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>>>,
}

impl EnvironmentProviders {
    /// Workdir-tier providers rooted at `sandbox_root` until startup installs
    /// an explicit Session provider.
    pub(in crate::host) fn new(
        sandbox_root: PathBuf,
        auxiliary_sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    ) -> Self {
        Self {
            auxiliary_sandboxes,
            session_provider: crate::session_environment::SessionEnvironmentProvider::workdir(
                sandbox_root,
            ),
            environment_checkpoint_store: None,
            cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer::default(),
            backend_owned_session_provider: None,
            session_provider_explicit: false,
            memory_mounter: std::sync::RwLock::new(None),
        }
    }
}

impl SharedHost {
    /// Install the sole checkpoint-byte adapter. This does not install lifecycle
    /// state; Session root CAS remains authoritative.
    #[must_use]
    pub fn with_environment_checkpoint_store(
        mut self,
        store: Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>,
    ) -> Self {
        self.env.environment_checkpoint_store = Some(store);
        self
    }

    /// Install the credential-file broker shared by every Session sandbox tier.
    /// Application plans retain opaque `MountSource::Secret` references; only
    /// the selected provider asks this port for bytes at realization time.
    #[must_use]
    pub fn with_session_secret_broker(
        self,
        broker: Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) -> Self {
        self.env
            .session_provider
            .install_secret_broker(broker.clone());
        if let Some(provider) = &self.env.backend_owned_session_provider {
            provider.install_secret_broker(broker);
        }
        self
    }

    /// Install the already-configured provider that owns every Session container
    /// environment on this Host.
    ///
    /// This is the public startup seam for downstream providers. The Host
    /// continues to own Session routing and lifecycle; the provider supplies only
    /// its enforceable Sandbox capabilities and create/adopt implementation. A
    /// caller that also installs a [`awaken_provisioning_contract::SecretBroker`]
    /// must do so after selecting the provider so there is one broker installation
    /// on the authoritative environment path.
    #[must_use]
    pub fn with_session_container_provider(
        self,
        provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
        hand_factory: Arc<dyn awaken_session_environment_contract::HandExecutorFactory>,
    ) -> Self {
        self.with_session_container_provider_and_capacity(provider, None, hand_factory)
    }

    /// Install one container provider and its optional never-used-capacity owner.
    /// Keeping both handles from the same startup prevents provider erasure
    /// from orphaning startup warmup and shutdown drain.
    #[must_use]
    pub fn with_session_container_provider_and_capacity(
        mut self,
        provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_container_contract::ContainerEnvironmentCapacity>>,
        hand_factory: Arc<dyn awaken_session_environment_contract::HandExecutorFactory>,
    ) -> Self {
        let hand_bin = self.deployment.sandbox.container_hand_bin.clone();
        self.env.session_provider = crate::session_environment::SessionEnvironmentProvider::
            container_with_capacity_hand_idle_and_residency(
                provider,
                capacity,
                Vec::new(),
                hand_factory,
                hand_bin,
                std::time::Duration::from_secs(self.deployment.sandbox.container_hand_idle_secs),
                self.deployment.sandbox.container_hand_residency,
                self.clock.clone(),
            );
        self.env.session_provider_explicit = true;
        if let Some(mounter) = self.memory_mounter() {
            self.env.session_provider.install_memory_mounter(mounter);
        }
        self
    }

    /// Install the complete result of the canonical deployment builder after a
    /// downstream composition has decorated its provider. Capacity, creation
    /// mounts, and cache initialization remain paired with that same provider
    /// construction instead of being rebuilt on a parallel path.
    #[must_use]
    pub fn with_session_container_environment_components(
        mut self,
        components: ContainerEnvironmentComponents,
        hand_factory: Arc<dyn awaken_session_environment_contract::HandExecutorFactory>,
    ) -> Self {
        let hand_bin = self.deployment.sandbox.container_hand_bin.clone();
        self.env.session_provider = crate::session_environment::SessionEnvironmentProvider::
            container_with_capacity_hand_idle_and_residency(
                components.provider,
                components.capacity,
                components.extra_mounts,
                hand_factory,
                hand_bin,
                std::time::Duration::from_secs(
                    self.deployment.sandbox.container_hand_idle_secs,
                ),
                self.deployment.sandbox.container_hand_residency,
                self.clock.clone(),
            );
        if let Some(initializer) = components.cache_volume_initializer {
            self.env.cache_volume_prewarmer =
                crate::cache_volume::CacheVolumePrewarmer::new(initializer);
        }
        self.env.session_provider_explicit = true;
        if let Some(mounter) = self.memory_mounter() {
            self.env.session_provider.install_memory_mounter(mounter);
        }
        self
    }

    /// Install the process composition's auxiliary-run sandbox factory. Every
    /// auxiliary Agent Run (compactor, Memory selection/extraction, isolated
    /// Skill fork) realizes its fresh sandbox through it; a Host without one
    /// refuses those Runs instead of choosing a sandbox implementation itself.
    #[must_use]
    pub fn with_auxiliary_sandboxes(
        mut self,
        factory: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    ) -> Self {
        self.resources
            .memory
            .set_auxiliary_sandbox_provider(factory.provider("mem"));
        self.env.auxiliary_sandboxes = factory;
        self
    }

    /// Whether process startup already installed the authoritative Session
    /// provider. The container-tier composition installs its provider only
    /// when none is; deployment selection likewise preserves an installed one.
    #[must_use]
    pub fn session_environment_provider_installed(&self) -> bool {
        self.env.session_provider_explicit
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
}
