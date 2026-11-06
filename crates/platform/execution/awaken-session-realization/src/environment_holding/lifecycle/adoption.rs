// @formal surface obligation=session_environment.owner_topology_is_exact_and_frozen
// @formal reason: A transient validated adoption input selects at most one retained wrapper; canonical ownership stays in the installed Session.
//! Mutually exclusive sources for reusing a validated Environment wrapper.

use crate::environment_owner::UnboundSessionEnvironment;
use crate::session_environment::SessionEnvironment;
use std::sync::Arc;

/// The exact binding/spec validated before adoption observes the provider.
/// This value borrows no authority and does not install or transfer ownership.
pub struct ValidatedSessionEnvironmentAdoption {
    pub(super) handle: awaken_provisioning_contract::SandboxHandle,
    pub(super) spec: awaken_provisioning_contract::SandboxSpec,
    pub(super) retained: Option<RetainedAdoptionEnvironment>,
}

pub(crate) enum RetainedAdoptionEnvironment {
    Existing(Arc<SessionEnvironment>),
    Candidate(Box<UnboundSessionEnvironment>),
}

impl RetainedAdoptionEnvironment {
    pub(crate) fn environment(&self) -> &Arc<SessionEnvironment> {
        match self {
            Self::Existing(environment) => environment,
            Self::Candidate(candidate) => &candidate.environment,
        }
    }

    /// Terminal preparation may carry a previously committed Candidate's
    /// identity; an ordinary unpublished Candidate supplies only its wrapper.
    pub(crate) fn into_committed_candidate(self) -> Option<UnboundSessionEnvironment> {
        match self {
            Self::Candidate(candidate) if candidate.durable_identity().is_some() => {
                Some(*candidate)
            }
            _ => None,
        }
    }
}

impl ValidatedSessionEnvironmentAdoption {
    /// The selected wrapper keeps its original allocation; validation alone
    /// neither publishes a Candidate nor replaces the installed owner.
    #[must_use]
    pub fn retained_environment(&self) -> Option<&Arc<SessionEnvironment>> {
        self.retained
            .as_ref()
            .map(RetainedAdoptionEnvironment::environment)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        awaken_provisioning_contract::SandboxHandle,
        awaken_provisioning_contract::SandboxSpec,
        Option<RetainedAdoptionEnvironment>,
    ) {
        (self.handle, self.spec, self.retained)
    }
}
