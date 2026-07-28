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
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
    },
}

impl SessionEnvironmentProvider {
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

    pub(crate) fn namespace(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Namespace(NamespaceProvider::new(base))
    }

    pub(crate) fn container(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
    ) -> Self {
        Self::Container {
            provider,
            extra_mounts,
            hand_factory,
        }
    }

    pub(crate) fn at_root(&self, base: impl Into<std::path::PathBuf>) -> Self {
        let base = base.into();
        match self {
            Self::Workdir(_) => Self::workdir(base),
            Self::Namespace(_) => Self::namespace(base),
            Self::Container {
                provider,
                extra_mounts,
                hand_factory,
            } => Self::Container {
                provider: provider.clone(),
                extra_mounts: extra_mounts.clone(),
                hand_factory: hand_factory.clone(),
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
            } => {
                let mut spec = spec.clone();
                spec.isolation = pc::IsolationClass::Container;
                spec.mounts.extend(extra_mounts.iter().cloned());
                for mount in &mut spec.mounts {
                    if !mount.mount_path.starts_with('/') {
                        mount.mount_path = container_files::workspace_path(&mount.mount_path)?;
                    }
                }
                let environment = provider.create_environment(&spec).await?;
                SessionEnvironment::container(environment, hand_factory.as_ref()).await
            }
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
                ..
            } => {
                let environment = provider.adopt_environment(handle).await?;
                environment.renew_lease().await?;
                SessionEnvironment::container(environment, hand_factory.as_ref()).await
            }
        }
    }
}
