//! Dependency-inverted container runtime port and its shared value types.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;

use crate::ContainerPlan;

/// A no-bypass claim is publishable only when both independent deployment
/// evidence sources exist: runtime isolation and a capability issuer.
pub(crate) const fn allowlist_capability_advertised(
    runtime_attested: bool,
    issuer_installed: bool,
) -> bool {
    runtime_attested && issuer_installed
}

/// Capabilities common to one concrete container runtime. Network denial is
/// runtime evidence rather than an isolation-class assumption.
pub(crate) fn container_capabilities(
    network_isolation: bool,
    enforced_network_allowlist: bool,
    package_provisioning: bool,
    control_services: std::collections::BTreeSet<SandboxControlServiceKind>,
) -> pc::SandboxCapabilities {
    pc::SandboxCapabilities {
        isolation: pc::IsolationClass::Container,
        tool_transparent: true,
        path_fidelity: true,
        enforced_readonly: true,
        network_isolation,
        enforced_network_allowlist,
        secret_egress_substitution: false,
        resource_limits: true,
        custom_rootfs: true,
        package_provisioning,
        control_services,
    }
}

#[cfg(test)]
mod capability_tests {
    use super::allowlist_capability_advertised;

    #[test]
    fn allowlist_claim_requires_runtime_and_issuer() {
        assert!(!allowlist_capability_advertised(false, false));
        assert!(!allowlist_capability_advertised(false, true));
        assert!(!allowlist_capability_advertised(true, false));
        assert!(allowlist_capability_advertised(true, true));
    }
}

#[cfg(kani)]
#[kani::proof]
fn allowlist_claim_never_exceeds_its_evidence() {
    let runtime_attested: bool = kani::any();
    let issuer_installed: bool = kani::any();
    let advertised = allowlist_capability_advertised(runtime_attested, issuer_installed);
    assert!(!advertised || (runtime_attested && issuer_installed));
}

/// A memory-store mount carried into a remote container runtime. The canonical
/// MemoryMounter seeds and harvests these bytes; the Pod receives no Resource
/// authority credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMount {
    pub store_id: String,
    pub mount_path: String,
    pub access: pc::MountAccess,
    pub snapshot_tar: Vec<u8>,
}

/// Deployment-owned policy for a Kubernetes Session's retained active volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sContinuationVolume {
    pub storage_class_name: Option<String>,
    pub size: String,
}

/// A container/pod runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("container {0:?} not found")]
    NotFound(String),
    #[error("container runtime failed: {0}")]
    Backend(String),
}

/// Whether a container is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Provisioning,
    Running,
    Gone,
}

/// Whether a provider is binding a newly realized runtime object or proving an
/// adopted object against durable incarnation evidence. The two states are
/// deliberately distinct so adoption can never mint trust from a same-name
/// ambient object.
#[derive(Debug, Clone, Copy)]
pub enum SandboxControlBindingRequest<'a> {
    New {
        required: &'a std::collections::BTreeSet<SandboxControlServiceKind>,
    },
    Adopt {
        required: &'a std::collections::BTreeSet<SandboxControlServiceKind>,
        expected: Option<&'a pc::SandboxControlIncarnation>,
    },
}

impl SandboxControlBindingRequest<'_> {
    #[must_use]
    pub fn required(&self) -> &std::collections::BTreeSet<SandboxControlServiceKind> {
        match self {
            Self::New { required } | Self::Adopt { required, .. } => required,
        }
    }
}

/// One opaque agent process started inside an already-running container.
pub struct RuntimeAgentProcess {
    pub process: Box<dyn pc::ProcessHandle>,
    pub channel: Box<dyn AgentChannel>,
}

/// Independent package-image build/publish port.
#[async_trait]
pub trait PackageImageProvisioner: Send + Sync {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        Ok(reference.to_owned())
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError>;

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
        _image: &str,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

/// Runtime seam driven by the provider and implemented by Docker, Podman, K8s,
/// or the test fake. It owns no Session or application policy.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    fn enforces_network_none(&self) -> bool {
        false
    }

    /// Live evidence that arbitrary workload traffic can reach the public
    /// network only through the configured allowlist proxy.
    fn enforces_network_allowlist(&self) -> bool {
        false
    }

    /// Revalidate backend availability and mutable external enforcement.
    /// Every adapter must name its evidence source; readiness has no permissive
    /// default that could accidentally advertise stale authority.
    async fn probe_ready(&self) -> Result<(), RuntimeError>;

    fn supports_package_provisioning(&self) -> bool {
        false
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            Ok(base_image.to_string())
        } else {
            Err(RuntimeError::Backend(
                "container runtime cannot provision package requirements".into(),
            ))
        }
    }

    fn has_native_memory_mounts(&self) -> bool {
        false
    }

    fn uses_persistent_volume_claims(&self) -> bool {
        false
    }

    fn supports_secret_writeback(&self) -> bool {
        true
    }

    /// Closed provider capability set. An empty default makes Docker, Podman,
    /// and out-of-tree runtimes fail admission without a compatibility fallback.
    fn sandbox_control_services(&self) -> std::collections::BTreeSet<SandboxControlServiceKind> {
        std::collections::BTreeSet::new()
    }

    async fn sandbox_control_binding(
        &self,
        _container_id: &str,
        request: SandboxControlBindingRequest<'_>,
    ) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
        if request.required().is_empty() {
            Ok(None)
        } else {
            Err(RuntimeError::Backend(
                "container runtime cannot bind a Sandbox control service incarnation".into(),
            ))
        }
    }

    async fn open_sandbox_control_channel(
        &self,
        _container_id: &str,
        _binding: &pc::SandboxControlIncarnation,
        _kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not publish the requested Sandbox control service".into(),
        ))
    }

    fn uses_host_live_input_bind(&self) -> bool {
        false
    }

    fn supports_live_input_projection(&self) -> bool {
        self.uses_host_live_input_bind()
    }

    async fn project_live_input(
        &self,
        _container_id: &str,
        _path: &str,
        _bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not support live input projection".into(),
        ))
    }

    async fn remove_live_input(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not support live input projection".into(),
        ))
    }

    async fn read_live_file(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        Ok(None)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError>;

    /// Runtime-owned, non-secret incarnation evidence persisted inside the
    /// canonical SandboxHandle. Most runtimes need none; Kubernetes uses it to
    /// fence retained-volume deletion across Worker replacement.
    async fn handle_extra(
        &self,
        _container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        Ok(None)
    }

    async fn spawn(
        &self,
        _container_id: &str,
        _command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement exec".into(),
        ))
    }

    async fn spawn_agent(
        &self,
        _container_id: &str,
        _command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement attached exec".into(),
        ))
    }

    async fn process(
        &self,
        _container_id: &str,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime cannot reconnect to exec process".into(),
        ))
    }

    async fn open_channel(&self, container_id: &str)
    -> Result<Box<dyn AgentChannel>, RuntimeError>;
    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError>;
    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError>;
    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError>;
    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError>;
    async fn artifacts(&self, container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError>;
    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError>;
    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError>;
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError>;

    async fn remove_with_handle(
        &self,
        container_id: &str,
        _runtime_handle: Option<&pc::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        self.remove(container_id).await
    }
}
