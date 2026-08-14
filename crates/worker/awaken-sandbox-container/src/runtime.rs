//! Dependency-inverted container runtime port and its shared value types.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;

use crate::ContainerPlan;

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

    async fn package_image_available(&self, _image: &str) -> Result<bool, RuntimeError> {
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
    ) -> Result<Option<serde_json::Value>, RuntimeError> {
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
        _runtime_handle: Option<&serde_json::Value>,
    ) -> Result<(), RuntimeError> {
        self.remove(container_id).await
    }
}
