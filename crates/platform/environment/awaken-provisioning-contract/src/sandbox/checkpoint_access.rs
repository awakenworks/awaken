// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: current-authority access for one checkpoint upload participant.
// @formal reason: Dynamic path: admit only an exact successor of the original upload fence under the
// @formal reason: current authority; the participant state machine stays in the provider.
//! Physical checkpoint participant access. Archive and object custody stay at
//! the calling codec/store; Session remains the only lifecycle authority.

use super::{SandboxCheckpointRef, SandboxCheckpointUpload, SandboxEffectFence, SandboxError};

/// Immutable original upload evidence and an independently current grant.
/// The source fence may have expired; only `authority` grants an effect.
#[derive(Clone, Copy)]
pub struct SandboxCheckpointAccess<'a> {
    pub source_effect_fence: &'a SandboxEffectFence,
    pub effect_fence: &'a SandboxEffectFence,
    pub request_fingerprint: &'a str,
    pub authority: &'a dyn crate::CurrentExecutionAuthority,
}

impl SandboxCheckpointAccess<'_> {
    /// Admit this access: a non-empty request, an exact successor of the
    /// original fence (valid by construction), and the current authority for
    /// that successor.
    pub fn admit(&self) -> Result<(), SandboxError> {
        if self.request_fingerprint.trim().is_empty()
            || !self
                .source_effect_fence
                .authorizes_successor(self.effect_fence)
        {
            return Err(SandboxError::invalid("invalid checkpoint access"));
        }
        crate::with_current_sandbox_effect(self.effect_fence, self.authority, |_| Ok(()))
    }

    /// Whether this access continues an upload under a different (terminal)
    /// operation than the one that began it.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.source_effect_fence.operation_id() != self.effect_fence.operation_id()
    }
}

/// One physical participant, not an object-storage or Session command. Acquire
/// is also the current-authority/physical-identity check before and after I/O.
#[derive(Clone, Copy)]
pub enum SandboxCheckpointMutation<'a> {
    Acquire,
    Begin(&'a SandboxCheckpointUpload),
    Complete(&'a SandboxCheckpointRef),
}
