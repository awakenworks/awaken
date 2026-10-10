// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Carries the original borrowed aggregate grant at the container boundary;
// @formal reason: admission delegates to the existing current Sandbox effect authority.
//! Borrowed effect ownership; no lease, queue or execution state is owned here.

use awaken_provisioning_contract as pc;

/// Who owns one container realization effect. A fence travels together with
/// the current authority that admits it, so a fence without authority, or an
/// omitted fence that silently selects the unfenced path, cannot be written.
#[derive(Clone, Copy)]
pub enum ContainerEffect<'a> {
    /// No aggregate effect owns the environment.
    Unattached,
    /// An aggregate-authorized effect and its current authority.
    Fenced {
        fence: &'a pc::SandboxEffectFence,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    },
}

impl<'a> ContainerEffect<'a> {
    pub const fn fence(self) -> Option<&'a pc::SandboxEffectFence> {
        match self {
            Self::Unattached => None,
            Self::Fenced { fence, .. } => Some(fence),
        }
    }

    /// Enter current authority for a fenced effect; an unattached effect has
    /// no authority to enter.
    pub fn admit(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Unattached => Ok(()),
            Self::Fenced { fence, authority } => {
                pc::with_current_sandbox_effect(fence, authority, |_| Ok(()))
            }
        }
    }
}

impl std::fmt::Debug for ContainerEffect<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.fence(), formatter)
    }
}
