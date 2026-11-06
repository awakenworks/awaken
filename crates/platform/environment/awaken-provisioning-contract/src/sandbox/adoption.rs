// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Names what one provider realization replaces and the canonical inputs of an adoption or
// @formal reason: observation; neither authorizes a mutation. This is a product-requirement
// @formal reason: classification, not a formal-proof claim.
//! What one realization replaces, and the canonical inputs to reopen or
//! observe one durable Sandbox handle.

use super::{
    SandboxEffect, SandboxEffectFence, SandboxError, SandboxHandle, SandboxObservationBasis,
};
use crate::spec::SandboxSpec;
use crate::vocab::{EnvVar, NetworkPolicy};

/// Which physical realization one Session Environment effect asks a provider
/// for. Consumer: the Session owner's create path, which knows only the
/// already-authorized source handle; the provider interprets its own payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxRealizationIntent {
    /// Realize a new physical environment.
    Create,
    /// Replace only the exact physical incarnation recorded in the
    /// aggregate-authorized `source` handle. Stable scope and lease age alone
    /// are never deletion authority; the provider decodes the incarnation (and
    /// any runtime continuation evidence) from this handle and refuses one it
    /// cannot interpret.
    Rebuild { source: Box<SandboxHandle> },
}

impl SandboxRealizationIntent {
    /// The handle a `Rebuild` replaces; a `Create` replaces nothing.
    #[must_use]
    pub fn source(&self) -> Option<&SandboxHandle> {
        match self {
            Self::Create => None,
            Self::Rebuild { source } => Some(source),
        }
    }

    /// A `Rebuild` replaces a physical incarnation the aggregate owns, so it
    /// is admitted only under a fenced effect; an unattached rebuild is
    /// refused before any provider effect. A `Create` is admitted under
    /// either.
    pub fn admit_under(&self, effect: SandboxEffect<'_>) -> Result<(), SandboxError> {
        match (self, effect) {
            (Self::Rebuild { .. }, SandboxEffect::Unattached) => Err(SandboxError::fence_rejected(
                "a Sandbox rebuild requires an aggregate effect fence",
            )),
            _ => Ok(()),
        }
    }
}

/// Ephemeral lifecycle witness for one in-process create attempt. It is stamped
/// on the backend object only to distinguish response loss in this call (whose
/// host staging and Memory handles are still alive) from a prior process/call
/// that must be recreated. It is never a durable identity or deletion fence.
///
/// The provider that creates the backend object mints the value; consumers
/// only compare and borrow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCreateAttempt(String);

impl SandboxCreateAttempt {
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

/// What absence must establish for the caller. Recovery may retain durable
/// continuation bytes; terminal cleanup must never acknowledge those bytes as
/// gone merely because their execution process disappeared. Neither purpose
/// authorizes a mutation or substitutes for the aggregate effect fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxObservationPurpose {
    Recovery,
    TerminalCleanup,
    /// Read original participant retention without adopting or disposing it.
    /// Prepared means the Session root already authorized its Dispose phase;
    /// this observation grants neither cleanup authority nor an exit receipt.
    WorkerRetirement {
        source_prepared: bool,
    },
}

/// Canonical inputs required to reattach or observe one existing Sandbox.
///
/// The durable handle identifies the physical object; the frozen Sandbox
/// specification remains the authority for security-sensitive mount and
/// writable-root policy. Keeping both inputs in one typed request prevents
/// adapters from reconstructing policy from an intentionally minimal handle.
#[derive(Debug, Clone, Copy)]
pub struct SandboxAdoption<'a> {
    pub spec: &'a SandboxSpec,
    pub handle: &'a SandboxHandle,
    pub retained_attempt: Option<&'a SandboxCreateAttempt>,
    /// Observation semantics only; never authorizes adoption or deletion.
    pub observation_purpose: SandboxObservationPurpose,
}

impl<'a> SandboxAdoption<'a> {
    #[must_use]
    pub const fn new(spec: &'a SandboxSpec, handle: &'a SandboxHandle) -> Self {
        Self {
            spec,
            handle,
            retained_attempt: None,
            observation_purpose: SandboxObservationPurpose::Recovery,
        }
    }

    /// Validate the effect observation's asserted spec against this exact
    /// adoption request. Providers reuse this single pre-I/O decision; it
    /// returns only the original borrowed fence and grants no effect authority.
    pub fn observation_fence<'b>(
        &self,
        basis: SandboxObservationBasis<'b>,
    ) -> Result<Option<&'b SandboxEffectFence>, SandboxError> {
        match basis {
            SandboxObservationBasis::Unattached => Ok(None),
            SandboxObservationBasis::Effect { spec, effect_fence } if spec == self.spec => {
                Ok(Some(effect_fence))
            }
            SandboxObservationBasis::Effect { .. } => Err(SandboxError::invalid(
                "Sandbox observation basis names a different effective spec",
            )),
        }
    }

    /// The observation basis for revalidating this adoption at an
    /// aggregate-authorized effect boundary: its own effective spec and `fence`.
    #[must_use]
    pub const fn effect_basis(
        &self,
        effect_fence: &'a SandboxEffectFence,
    ) -> SandboxObservationBasis<'a> {
        SandboxObservationBasis::Effect {
            spec: self.spec,
            effect_fence,
        }
    }
}

/// The layout a provider reopens from one of its durable handles: the outputs
/// root and the base process environment frozen at creation, and the network
/// policy when the provider freezes one into the handle (`None` keeps the
/// effective spec's). Consumer: the Session owner's pure adoption-layout
/// preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxAdoptionLayout {
    pub outputs_path: String,
    pub base_env: Vec<EnvVar>,
    pub network: Option<NetworkPolicy>,
}

impl SandboxAdoptionLayout {
    /// Reopen `spec` with this layout's historical coordinates.
    #[must_use]
    pub fn apply_to(&self, mut spec: SandboxSpec) -> SandboxSpec {
        spec.outputs_path.clone_from(&self.outputs_path);
        spec.env.clone_from(&self.base_env);
        if let Some(network) = &self.network {
            spec.network.clone_from(network);
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_requires_a_fenced_effect_and_names_its_source() {
        // | Rule | intent  | effect     | admit_under      | source |
        // | I1   | Create  | Unattached | Ok               | None   |
        // | I2   | Create  | Fenced     | Ok               | None   |
        // | I3   | Rebuild | Unattached | FenceRejected    | handle |
        // | I4   | Rebuild | Fenced     | Ok               | handle |
        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, u64::MAX).unwrap();
        let authority = crate::test_support::EffectAuthority::new(&fence);
        let fenced = SandboxEffect::Fenced {
            fence: &fence,
            authority: &authority,
        };
        let handle = SandboxHandle::new("fake", "scope");
        let rebuild = SandboxRealizationIntent::Rebuild {
            source: Box::new(handle.clone()),
        };
        assert!(
            SandboxRealizationIntent::Create
                .admit_under(SandboxEffect::Unattached)
                .is_ok(),
            "I1"
        );
        assert!(
            SandboxRealizationIntent::Create.admit_under(fenced).is_ok(),
            "I2"
        );
        assert!(SandboxRealizationIntent::Create.source().is_none(), "I1/I2");
        assert!(
            matches!(
                rebuild.admit_under(SandboxEffect::Unattached),
                Err(SandboxError::FenceRejected(_))
            ),
            "I3"
        );
        assert!(rebuild.admit_under(fenced).is_ok(), "I4");
        assert_eq!(rebuild.source(), Some(&handle), "I3/I4");
    }

    #[test]
    fn adoption_layout_reopens_only_its_historical_coordinates() {
        // L1 outputs and base env always replace the effective spec's;
        // L2 a frozen network replaces it; L3 no frozen network keeps it.
        let spec = crate::test_support::spec("scope", None);
        let env = vec![EnvVar {
            name: "A".into(),
            value: crate::vocab::EnvValue::Inline { value: "1".into() },
            visibility: crate::vocab::EnvVisibility::Process,
        }];
        let mut layout = SandboxAdoptionLayout {
            outputs_path: "/historical".into(),
            base_env: env.clone(),
            network: None,
        };
        let reopened = layout.apply_to(spec.clone());
        assert_eq!(reopened.outputs_path, "/historical", "L1");
        assert_eq!(reopened.env, env, "L1");
        assert_eq!(reopened.network, spec.network, "L3");
        layout.network = Some(NetworkPolicy::None);
        assert_eq!(layout.apply_to(spec).network, NetworkPolicy::None, "L2");
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[test]
    fn observation_basis_preserves_the_exact_fence_only_for_the_requested_spec() {
        // Causes: C1 unattached/effect observation; C2 effect assertion uses
        // identical values (same reference or an equal clone) versus a changed
        // effective spec. Effects: O1 unattached -> no fence; O2 equal spec ->
        // the ORIGINAL borrowed fence; O3 changed spec -> Invalid before I/O.
        // Rows: U (no asserted spec exists), E1 same reference, E2 equal clone,
        // E3 changed outputs. This pure preflight does not parse a backend
        // payload, call current authority, mutate state or mint a fresh fence.
        let spec = crate::test_support::spec("observation", None);
        let equal = spec.clone();
        let mut foreign = spec.clone();
        foreign.outputs_path = "/different".into();
        let handle = SandboxHandle::new("fake", "observation");
        let adoption = SandboxAdoption::new(&spec, &handle);
        let fence = SandboxEffectFence::new("observe", "owner", "boot", 1, u64::MAX).unwrap();
        assert!(
            adoption
                .observation_fence(SandboxObservationBasis::Unattached)
                .unwrap()
                .is_none(),
            "U"
        );
        for asserted in [&spec, &equal] {
            let actual = adoption
                .observation_fence(SandboxObservationBasis::Effect {
                    spec: asserted,
                    effect_fence: &fence,
                })
                .unwrap()
                .unwrap();
            assert!(
                std::ptr::eq(actual, &fence),
                "E1/E2 original borrowed fence"
            );
        }
        assert!(
            matches!(
                adoption.observation_fence(SandboxObservationBasis::Effect {
                    spec: &foreign,
                    effect_fence: &fence
                }),
                Err(SandboxError::Invalid(_))
            ),
            "E3"
        );
    }
}
