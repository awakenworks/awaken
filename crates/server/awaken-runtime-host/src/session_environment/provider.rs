//! Selection and lifecycle construction for a Session's single environment.

use super::{HandExecutorFactory, SessionEnvironment, container_files};
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

/// Creates/adopts the one Session environment while auxiliary housekeeping Runs
/// may continue using their deliberately-fresh LocalProvider.
pub(crate) enum SessionEnvironmentProvider {
    Workdir(LocalProvider),
    Namespace(NamespaceProvider),
    Container {
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        hand_idle_after: std::time::Duration,
    },
}

impl SessionEnvironmentProvider {
    /// Construct the in-process adapter for a non-container Deployment tier.
    /// Both initial Host construction and async composition use this one mapping;
    /// container tiers return `None` because their provider requires async setup.
    pub(crate) fn for_host_tier(
        tier: crate::SandboxTier,
        base: impl Into<std::path::PathBuf>,
        inherit_agent_stderr: bool,
    ) -> Option<Self> {
        let base = base.into();
        match tier {
            crate::SandboxTier::Local => {
                Some(Self::workdir_with_agent_stderr(base, inherit_agent_stderr))
            }
            crate::SandboxTier::Namespace => Some(Self::namespace_with_agent_stderr(
                base,
                inherit_agent_stderr,
            )),
            crate::SandboxTier::Docker | crate::SandboxTier::Podman | crate::SandboxTier::K8s => {
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir(_))
    }

    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir(provider) => pc::SandboxProvider::capabilities(provider),
            Self::Namespace(provider) => pc::SandboxProvider::capabilities(provider),
            Self::Container { provider, .. } => provider.sandbox_capabilities(),
        }
    }

    pub(crate) fn workdir(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Workdir(LocalProvider::new(base))
    }

    pub(crate) fn workdir_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Workdir(LocalProvider::new(base).with_agent_stderr(inherit))
    }

    pub(crate) fn namespace_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Namespace(NamespaceProvider::new(base).with_agent_stderr(inherit))
    }

    #[cfg(test)]
    pub(crate) fn container(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
    ) -> Self {
        Self::container_with_capacity_and_hand_idle(
            provider,
            None,
            extra_mounts,
            hand_factory,
            hand_bin,
            std::time::Duration::ZERO,
        )
    }

    pub(crate) fn container_with_capacity_and_hand_idle(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        Self::Container {
            provider,
            capacity,
            extra_mounts,
            hand_factory,
            hand_bin: hand_bin.into(),
            hand_idle_after,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn at_root(&self, base: impl Into<std::path::PathBuf>) -> Self {
        let base = base.into();
        match self {
            Self::Workdir(provider) => {
                Self::workdir_with_agent_stderr(base, provider.inherits_agent_stderr())
            }
            Self::Namespace(provider) => {
                Self::namespace_with_agent_stderr(base, provider.inherits_agent_stderr())
            }
            Self::Container {
                provider,
                capacity,
                extra_mounts,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => Self::Container {
                provider: provider.clone(),
                capacity: capacity.clone(),
                extra_mounts: extra_mounts.clone(),
                hand_factory: hand_factory.clone(),
                hand_bin: hand_bin.clone(),
                hand_idle_after: *hand_idle_after,
            },
        }
    }

    pub(crate) fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        match self {
            Self::Workdir(provider) => provider.install_memory_mounter(mounter),
            Self::Namespace(provider) => provider.install_memory_mounter(mounter),
            Self::Container { provider, .. } => provider.install_memory_mounter(mounter),
        }
    }

    pub(crate) fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        match self {
            Self::Workdir(provider) => provider.install_secret_broker(broker),
            Self::Namespace(provider) => provider.install_secret_broker(broker),
            Self::Container { provider, .. } => provider.install_secret_broker(broker),
        }
    }

    pub(crate) async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => provider
                .create_sandbox(spec)
                .await
                .map(SessionEnvironment::workdir),
            Self::Namespace(provider) => {
                let mut spec = spec.clone();
                spec.isolation = pc::IsolationClass::Namespace;
                provider
                    .create_sandbox(&spec)
                    .await
                    .map(SessionEnvironment::namespace)
            }
            Self::Container {
                provider,
                extra_mounts,
                hand_factory,
                hand_bin,
                hand_idle_after,
                ..
            } => {
                let capabilities = provider.sandbox_capabilities();
                let spec = container_spec(spec, extra_mounts)?;
                let environment = provider.create_environment(&spec).await?;
                SessionEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    capabilities,
                )
                .await
            }
        }
    }

    /// Pre-create never-used capacity for the same exact normalized container
    /// shape that [`Self::create`] will request. Non-container and direct-provider
    /// deployments have no capacity owner and return zero.
    pub(crate) async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        match self {
            Self::Container {
                capacity: Some(capacity),
                extra_mounts,
                ..
            } => {
                let spec = container_spec(spec, extra_mounts)?;
                capacity.prewarm_to(&spec, target).await
            }
            _ => Ok(0),
        }
    }

    pub(crate) async fn discard_capacity(&self, spec: &pc::SandboxSpec) {
        if let Self::Container {
            capacity: Some(capacity),
            extra_mounts,
            ..
        } = self
            && let Ok(spec) = container_spec(spec, extra_mounts)
        {
            capacity.discard_shape(&spec).await;
        }
    }

    pub(crate) fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize {
        if let Self::Container {
            capacity: Some(capacity),
            extra_mounts,
            ..
        } = self
            && let Ok(spec) = container_spec(spec, extra_mounts)
        {
            return capacity.ready_capacity(&spec);
        }
        0
    }

    /// Drain only unused container capacity. Active Session environments are no
    /// longer members of the pool and retain their ordinary Session lifecycle.
    pub(crate) async fn shutdown_capacity(&self) {
        if let Self::Container {
            capacity: Some(capacity),
            ..
        } = self
        {
            capacity.shutdown_capacity().await;
        }
    }

    pub(crate) async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => provider
                .adopt_sandbox(handle)
                .await
                .map(SessionEnvironment::workdir),
            Self::Namespace(provider) => provider
                .adopt_sandbox(handle)
                .await
                .map(SessionEnvironment::namespace),
            Self::Container {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
                ..
            } => {
                let capabilities = provider.sandbox_capabilities();
                let environment = provider.adopt_environment(handle).await?;
                environment.renew_lease().await?;
                SessionEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    capabilities,
                )
                .await
            }
        }
    }
}

fn container_spec(
    spec: &pc::SandboxSpec,
    extra_mounts: &[pc::MountRequirement],
) -> Result<pc::SandboxSpec, pc::SandboxError> {
    let mut spec = spec.clone();
    spec.isolation = pc::IsolationClass::Container;
    spec.mounts.extend(extra_mounts.iter().cloned());
    for mount in &mut spec.mounts {
        if !mount.mount_path.starts_with('/') {
            mount.mount_path = container_files::workspace_path(&mount.mount_path)?;
        }
    }
    Ok(spec)
}
