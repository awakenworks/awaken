// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Defines the live container environment port a Session owner drives; every
// @formal reason: capability is a required method, so an environment states its own refusal. This is a
// @formal reason: product-requirement classification, not a formal-proof claim.
//! The live container environment owned by one Session.

use awaken_provisioning_contract as pc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_sandbox_control::SandboxControlServicePublisher;

use super::{ContainerCheckpointAccess, ContainerCheckpointMutation};

/// Ephemeral lifecycle witness for one in-process create attempt. It is stamped
/// on the backend object only to distinguish response loss in this call (whose
/// host staging and Memory handles are still alive) from a prior process/call
/// that must be recreated. It is never a durable identity or deletion fence.
///
/// The provider that creates the backend object mints the value; consumers
/// only compare and borrow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerCreateAttempt(String);

impl ContainerCreateAttempt {
    /// Wrap the provider-minted attempt identity.
    #[must_use]
    pub fn new(attempt: impl Into<String>) -> Self {
        Self(attempt.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One opaque agent process started inside an already-running container.
pub struct RuntimeAgentProcess {
    pub process: Box<dyn pc::ProcessHandle>,
    pub channel: Box<dyn AgentChannel>,
}

/// One file harvested from a live container environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Object-safe live container environment owned by one Session.
#[async_trait]
pub trait ContainerEnvironment: pc::Sandbox + SandboxControlServicePublisher {
    /// Exact, retained process containment from the owning runtime. Absence is
    /// unsupported containment, not absence of effects. The tool owner persists
    /// this opaque reference before invoking an executable.
    async fn owned_process_binding(&self) -> Result<Option<String>, pc::SandboxError> {
        Ok(None)
    }

    /// Persist/replay the exact upload within this provider's physical owner.
    /// Does not capture bytes, upload objects, dispose, or commit Session state.
    /// Every method below is required: an environment without a capability
    /// states its own refusal instead of inheriting one.
    async fn checkpoint_upload(
        &self,
        access: ContainerCheckpointAccess<'_>,
        mutation: ContainerCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, pc::SandboxError>;

    /// Borrowed proof that the creating wrapper still owns its local participants.
    /// Never serialized in a SandboxHandle or reconstructed after process loss.
    fn retained_create_attempt(&self) -> Option<&ContainerCreateAttempt> {
        None
    }

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

    async fn remove_live_input_path(&self, path: &str) -> Result<(), pc::SandboxError>;

    /// Record one successfully materialized sandbox-visible tree in the
    /// provider's current V2 durable-handle evidence.
    fn record_owned_path(&self, path: &str) -> Result<(), pc::SandboxError>;

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError>;

    /// Open the resident agent's byte channel, when this environment keeps one.
    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, pc::SandboxError>;

    async fn read_files(&self, root: &str) -> Result<Vec<EnvironmentFile>, pc::SandboxError>;
}
