// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Defines the live Session-owned Sandbox port a Session owner drives; every capability
// @formal reason: that can fail is a required method, so a Sandbox states its own refusal. This is a
// @formal reason: product-requirement classification, not a formal-proof claim.
//! The live Sandbox one Session owns and runs its Agent in.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_sandbox_control::SandboxControlServicePublisher;

use super::{
    ProcessHandle, Sandbox, SandboxCheckpointAccess, SandboxCheckpointMutation,
    SandboxCheckpointUpload, SandboxCreateAttempt, SandboxError,
};
use crate::spec::Command;
use crate::vocab::MountRequirement;

/// One opaque agent process started inside an already-running Sandbox, with
/// the byte channel it speaks on.
pub struct AgentProcess {
    pub process: Box<dyn ProcessHandle>,
    pub channel: Box<dyn AgentChannel>,
}

/// One file harvested from a live Sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Object-safe live Sandbox owned by one Session: the Sandbox ports plus the
/// Agent process, live input, owned-path, checkpoint-upload and file
/// participants a Session owner drives, and the control services it publishes.
#[async_trait]
pub trait SessionSandbox: Sandbox + SandboxControlServicePublisher {
    /// Exact, retained process containment from the owning backend. Absence is
    /// unsupported containment, not absence of effects. The tool owner persists
    /// this opaque reference before invoking an executable. Required like
    /// every capability below: a Sandbox states its own absence.
    async fn owned_process_binding(&self) -> Result<Option<String>, SandboxError>;

    /// Persist/replay the exact upload within this provider's physical owner.
    /// Does not capture bytes, upload objects, dispose, or commit Session state.
    /// Every method below that can fail is required: a Sandbox without a
    /// capability states its own refusal instead of inheriting one.
    async fn checkpoint_upload(
        &self,
        access: SandboxCheckpointAccess<'_>,
        mutation: SandboxCheckpointMutation<'_>,
    ) -> Result<Option<SandboxCheckpointUpload>, SandboxError>;

    /// Borrowed proof that the creating wrapper still owns its local participants.
    /// Never serialized in a SandboxHandle or reconstructed after process loss.
    fn retained_create_attempt(&self) -> Option<&SandboxCreateAttempt> {
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
        _previous: &[MountRequirement],
        _next: &[MountRequirement],
    ) -> bool {
        false
    }

    async fn remove_live_input_path(&self, path: &str) -> Result<(), SandboxError>;

    /// Record one successfully materialized sandbox-visible tree in the
    /// provider's current V2 durable-handle evidence.
    fn record_owned_path(&self, path: &str) -> Result<(), SandboxError>;

    async fn spawn_agent_process(&self, command: Command) -> Result<AgentProcess, SandboxError>;

    /// Open the resident agent's byte channel, when this Sandbox keeps one.
    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, SandboxError>;

    async fn read_files(&self, root: &str) -> Result<Vec<SandboxFile>, SandboxError>;
}
