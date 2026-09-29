//! Who owns one provider effect, and the layout a provider will reopen.

use awaken_provisioning_contract as pc;

/// Who owns one Session Environment provider effect. The Session aggregate
/// decides this once (an unowned Environment has no realization lease); every
/// provider projection then takes the matching named operation. A fence always
/// travels with its current authority, so neither an omitted fence nor a fence
/// without authority can select a weaker path. Consumers: the Host's Session
/// realization and lifecycle, which pass it to the provider roles.
#[derive(Clone, Copy)]
pub enum ProviderEffect<'a> {
    /// No aggregate effect owns the Environment.
    Unattached,
    /// The aggregate-owned effect fence and the authority that admits it.
    Fenced {
        fence: &'a pc::SandboxEffectFence,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    },
}

impl<'a> ProviderEffect<'a> {
    /// Project an aggregate-derived optional fence and its captured authority.
    /// A fence without authority is refused rather than degraded.
    pub fn from_parts(
        fence: Option<&'a pc::SandboxEffectFence>,
        authority: Option<&'a dyn pc::CurrentExecutionAuthority>,
    ) -> Result<Self, pc::SandboxError> {
        match (fence, authority) {
            (Some(fence), Some(authority)) => Ok(Self::Fenced { fence, authority }),
            (None, _) => Ok(Self::Unattached),
            (Some(_), None) => Err(pc::SandboxError::fence_rejected(
                "a fenced Session Environment effect has no current authority",
            )),
        }
    }

    /// The observation basis this effect implies for `spec`.
    #[must_use]
    pub const fn observation_basis(
        self,
        spec: &'a pc::SandboxSpec,
    ) -> pc::SandboxObservationBasis<'a> {
        match self {
            Self::Unattached => pc::SandboxObservationBasis::Unattached,
            Self::Fenced { fence, .. } => pc::SandboxObservationBasis::Effect {
                spec,
                effect_fence: fence,
            },
        }
    }

    /// Admit the effect now: an unattached effect always, a fenced one only
    /// while its authority still holds the fence. Providers call this before
    /// and after every physical step.
    pub fn admit(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Unattached => Ok(()),
            Self::Fenced { fence, authority } => {
                pc::with_current_sandbox_effect(fence, authority, |_| Ok(()))
            }
        }
    }
}

/// The exact layout a provider will reopen from a durable handle: the
/// effective spec with the handle's historical output, environment and
/// network coordinates, and the paths the handle already owns. Consumer: the
/// Host's environment lifecycle, which rejects a stale conflicting layout
/// before any provider I/O.
pub struct EnvironmentAdoptionLayout {
    pub spec: pc::SandboxSpec,
    pub historical_owned_paths: Option<Vec<String>>,
}
