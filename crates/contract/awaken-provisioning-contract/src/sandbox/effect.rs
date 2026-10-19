// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Carries the original borrowed aggregate grant at the provider boundary;
// @formal reason: admission delegates to the existing current Sandbox effect authority.
//! Who owns one provider effect. Borrowed ownership only; no lease, queue or
//! execution state is owned here.

use super::{SandboxEffectFence, SandboxError, SandboxObservationBasis};
use crate::spec::SandboxSpec;

/// Who owns one Sandbox provider effect. The Session aggregate decides this
/// once (an unowned Environment has no realization lease); every provider
/// operation then receives the matching variant. A fence always travels with
/// its current authority, so neither an omitted fence nor a fence without
/// authority can select a weaker path.
#[derive(Clone, Copy)]
pub enum SandboxEffect<'a> {
    /// No aggregate effect owns the Sandbox: warm capacity, a capability
    /// probe, a cache-volume initializer, or an unowned Session.
    Unattached,
    /// The aggregate-owned effect fence and the authority that admits it.
    Fenced {
        fence: &'a SandboxEffectFence,
        authority: &'a dyn crate::CurrentExecutionAuthority,
    },
}

impl<'a> SandboxEffect<'a> {
    /// Project an aggregate-derived optional fence and its captured authority.
    /// A fence without authority is refused rather than degraded.
    pub fn from_parts(
        fence: Option<&'a SandboxEffectFence>,
        authority: Option<&'a dyn crate::CurrentExecutionAuthority>,
    ) -> Result<Self, SandboxError> {
        match (fence, authority) {
            (Some(fence), Some(authority)) => Ok(Self::Fenced { fence, authority }),
            (None, _) => Ok(Self::Unattached),
            (Some(_), None) => Err(SandboxError::fence_rejected(
                "a fenced Sandbox effect has no current authority",
            )),
        }
    }

    /// The fence this effect carries; an unattached effect has none.
    #[must_use]
    pub const fn fence(self) -> Option<&'a SandboxEffectFence> {
        match self {
            Self::Unattached => None,
            Self::Fenced { fence, .. } => Some(fence),
        }
    }

    /// The observation basis this effect implies for `spec`.
    #[must_use]
    pub const fn observation_basis(self, spec: &'a SandboxSpec) -> SandboxObservationBasis<'a> {
        match self {
            Self::Unattached => SandboxObservationBasis::Unattached,
            Self::Fenced { fence, .. } => SandboxObservationBasis::Effect {
                spec,
                effect_fence: fence,
            },
        }
    }

    /// Admit the effect now: an unattached effect always, a fenced one only
    /// while its authority still holds the fence. Providers call this before
    /// and after every physical step.
    pub fn admit(self) -> Result<(), SandboxError> {
        match self {
            Self::Unattached => Ok(()),
            Self::Fenced { fence, authority } => {
                crate::with_current_sandbox_effect(fence, authority, |_| Ok(()))
            }
        }
    }
}

impl std::fmt::Debug for SandboxEffect<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.fence(), formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_effect_is_named_and_never_degrades() {
        // Decision table for the one ownership projection.
        // | Rule | fence | authority | Effect                                  |
        // | P1   | none  | any       | Unattached; unattached observation      |
        // | P2   | some  | some      | Fenced; effect observation with spec    |
        // | P3   | some  | none      | FenceRejected, never Unattached         |
        // | P4   | some  | denied    | admit refuses; Unattached admits        |
        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, u64::MAX).unwrap();
        let authority = crate::test_support::EffectAuthority::new(&fence);
        let spec = crate::test_support::spec("scope", None);
        for authority in [
            None,
            Some(&authority as &dyn crate::CurrentExecutionAuthority),
        ] {
            let effect = SandboxEffect::from_parts(None, authority).expect("P1");
            assert!(matches!(effect, SandboxEffect::Unattached), "P1");
            assert!(effect.fence().is_none(), "P1 fence");
            assert!(
                matches!(
                    effect.observation_basis(&spec),
                    SandboxObservationBasis::Unattached
                ),
                "P1 basis"
            );
            assert!(effect.admit().is_ok(), "P1 admit");
        }
        let effect = SandboxEffect::from_parts(Some(&fence), Some(&authority)).expect("P2");
        assert_eq!(effect.fence(), Some(&fence), "P2 fence");
        assert!(
            matches!(
                effect.observation_basis(&spec),
                SandboxObservationBasis::Effect { spec: observed, effect_fence }
                    if observed == &spec && effect_fence == &fence
            ),
            "P2 basis"
        );
        assert!(effect.admit().is_ok(), "P2 admit");
        assert!(
            matches!(
                SandboxEffect::from_parts(Some(&fence), None),
                Err(SandboxError::FenceRejected(_))
            ),
            "P3"
        );
        let denied = crate::test_support::EffectAuthority::denied();
        assert!(
            SandboxEffect::Fenced {
                fence: &fence,
                authority: &denied
            }
            .admit()
            .is_err(),
            "P4"
        );
    }
}
