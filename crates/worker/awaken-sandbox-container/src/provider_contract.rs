//! Backend-erased contracts for Session-owned container environments.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServicePublisher;

use crate::RuntimeAgentProcess;

/// Object-safe live container environment owned by one Session.
#[async_trait]
pub trait ContainerEnvironment: pc::Sandbox + SandboxControlServicePublisher {
    fn outputs_path(&self) -> &str {
        "/outputs"
    }

    fn is_recovered(&self) -> bool {
        false
    }

    fn supports_live_mount_replacement(
        &self,
        _previous: &[pc::MountRequirement],
        _next: &[pc::MountRequirement],
    ) -> bool {
        false
    }

    async fn remove_live_input_path(&self, _path: &str) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new(
            "late mount removal is unsupported on this container tier",
        ))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError>;

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container environment does not expose a resident agent channel",
        ))
    }

    async fn read_files(&self, root: &str) -> Result<Vec<EnvironmentFile>, pc::SandboxError>;
}

/// One file harvested from a live container environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Canonical inputs required to reattach one existing container environment.
///
/// The durable handle identifies the runtime object; the frozen Sandbox
/// specification remains the authority for security-sensitive mount and
/// writable-root policy. Keeping both inputs in one typed request prevents
/// adapters from reconstructing policy from an intentionally minimal handle.
#[derive(Debug, Clone, Copy)]
pub struct ContainerEnvironmentAdoption<'a> {
    pub spec: &'a pc::SandboxSpec,
    pub handle: &'a pc::SandboxHandle,
}

impl<'a> ContainerEnvironmentAdoption<'a> {
    #[must_use]
    pub const fn new(spec: &'a pc::SandboxSpec, handle: &'a pc::SandboxHandle) -> Self {
        Self { spec, handle }
    }
}

/// Backend-erased provider for Session-owned container environments.
#[async_trait]
pub trait ContainerEnvironmentProvider: Send + Sync {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Workdir,
            tool_transparent: false,
            path_fidelity: false,
            enforced_readonly: false,
            network_isolation: false,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: Default::default(),
        }
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {}

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {}

    async fn probe_ready(&self) -> Result<(), pc::SandboxError>;

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    async fn adopt_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    async fn restore_environment(
        &self,
        _spec: &pc::SandboxSpec,
        _checkpoint: &pc::SandboxCheckpointRef,
        _store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement checkpoint restore",
        ))
    }
}
