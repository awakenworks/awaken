//! Strong process-local ownership state for one Session Environment.

use std::sync::Arc;

/// Exact origin of an Environment owner that predates generated durable
/// identity. Current Managed create/adopt/restore must be `Durable`, while
/// historical rows and non-Managed direct Threads remain explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LegacyDirectEnvironmentProvenance {
    Direct(awaken_session_contract::SessionEnvironmentReceipt),
    DurableBinding {
        binding: String,
        effect: LegacyEnvironmentEffect,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LegacyEnvironmentEffect {
    Known(String),
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BoundSessionEnvironmentIdentity {
    Durable {
        effect_id: String,
        generation: awaken_session_contract::SandboxGeneration,
    },
    LegacyDirect(LegacyDirectEnvironmentProvenance),
}

#[derive(Clone)]
pub(crate) struct UnboundSessionEnvironment {
    pub origin: UnboundSessionEnvironmentOrigin,
    pub binding: String,
    pub environment: Arc<crate::session_environment::SessionEnvironment>,
}

impl UnboundSessionEnvironment {
    pub(crate) fn exact_matches(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.binding == other.binding
            && Arc::ptr_eq(&self.environment, &other.environment)
            && self.environment.handle() == other.environment.handle()
    }

    pub(crate) fn requires_initial_provisioning(&self) -> bool {
        matches!(&self.origin, UnboundSessionEnvironmentOrigin::New)
    }

    pub(crate) fn effect_kind(&self) -> awaken_session_contract::SessionEnvironmentEffectKind {
        match &self.origin {
            UnboundSessionEnvironmentOrigin::New => {
                awaken_session_contract::SessionEnvironmentEffectKind::Create
            }
            UnboundSessionEnvironmentOrigin::Adoption
            | UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                awaken_session_contract::SessionEnvironmentEffectKind::Adopt
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnboundSessionEnvironmentOrigin {
    New,
    Adoption,
    DurableAdoption(BoundSessionEnvironmentIdentity),
}

#[derive(Clone)]
pub(crate) struct BoundSessionEnvironment {
    pub identity: BoundSessionEnvironmentIdentity,
    pub binding: String,
    pub environment: Arc<crate::session_environment::SessionEnvironment>,
}

impl BoundSessionEnvironment {
    pub(crate) fn exact_matches(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.binding == other.binding
            && Arc::ptr_eq(&self.environment, &other.environment)
            && self.environment.handle() == other.environment.handle()
    }
}

#[derive(Clone)]
pub(crate) enum SessionEnvironmentPreparation {
    AwaitingAdoption {
        identity: BoundSessionEnvironmentIdentity,
        binding: String,
    },
    Candidate(UnboundSessionEnvironment),
}

#[derive(Clone)]
pub(crate) enum SessionEnvironmentRestoration {
    Awaiting {
        request: awaken_session_contract::SandboxRestoreRequest,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnvironmentRetirementCause {
    UnpublishedCandidate,
    RecoveryDiscard,
    RealizationRevocation,
    Terminal {
        effect_id: String,
    },
    CheckpointSource {
        operation: awaken_session_contract::SessionEnvironmentOperation,
        generation: awaken_session_contract::SandboxGeneration,
    },
}

#[derive(Clone)]
pub(crate) struct RetiringSessionEnvironment {
    pub cause: SessionEnvironmentRetirementCause,
    pub owned: RetiringEnvironmentOwner,
}

impl RetiringSessionEnvironment {
    pub(crate) fn exact_matches(&self, other: &Self) -> bool {
        self.cause == other.cause && self.owned.exact_matches(&other.owned)
    }
}

#[derive(Clone)]
pub(crate) enum RetiringEnvironmentOwner {
    Unbound(UnboundSessionEnvironment),
    Bound(BoundSessionEnvironment),
}

impl RetiringEnvironmentOwner {
    pub(crate) fn exact_matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unbound(left), Self::Unbound(right)) => left.exact_matches(right),
            (Self::Bound(left), Self::Bound(right)) => left.exact_matches(right),
            (Self::Unbound(_), Self::Bound(_)) | (Self::Bound(_), Self::Unbound(_)) => false,
        }
    }

    pub(crate) fn binding(&self) -> &str {
        match self {
            Self::Unbound(owned) => &owned.binding,
            Self::Bound(owned) => &owned.binding,
        }
    }

    pub(crate) fn environment(&self) -> Arc<crate::session_environment::SessionEnvironment> {
        match self {
            Self::Unbound(owned) => owned.environment.clone(),
            Self::Bound(owned) => owned.environment.clone(),
        }
    }
}

/// The sole process-local Environment owner. Durable desired/lifecycle state
/// remains in the Session aggregate.
#[derive(Clone, Default)]
pub(crate) enum SessionEnvironmentOwner {
    #[default]
    Vacant,
    Preparing(SessionEnvironmentPreparation),
    Restoring(SessionEnvironmentRestoration),
    Resident(BoundSessionEnvironment),
    Retiring(RetiringSessionEnvironment),
}

impl SessionEnvironmentOwner {
    /// External/tool readers deliberately see only the Resident phase.
    pub(crate) fn resident(&self) -> Option<Arc<crate::session_environment::SessionEnvironment>> {
        match self {
            Self::Resident(owned) => Some(owned.environment.clone()),
            Self::Vacant | Self::Preparing(_) | Self::Restoring(_) | Self::Retiring(_) => None,
        }
    }

    pub(crate) fn is_resident(&self) -> bool {
        matches!(self, Self::Resident(_))
    }

    pub(crate) fn has_local_environment(&self) -> bool {
        matches!(
            self,
            Self::Preparing(SessionEnvironmentPreparation::Candidate(_))
                | Self::Resident(_)
                | Self::Retiring(_)
        )
    }

    /// Binding asserted by durable Session truth. An unpersisted candidate or
    /// direct Thread binding is intentionally excluded.
    pub(crate) fn durable_binding(&self) -> Option<&str> {
        fn identity_is_durable(identity: &BoundSessionEnvironmentIdentity) -> bool {
            matches!(
                identity,
                BoundSessionEnvironmentIdentity::Durable { .. }
                    | BoundSessionEnvironmentIdentity::LegacyDirect(
                        LegacyDirectEnvironmentProvenance::DurableBinding { .. }
                    )
            )
        }

        match self {
            Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                binding, ..
            }) => Some(binding),
            Self::Resident(owned) => {
                identity_is_durable(&owned.identity).then_some(owned.binding.as_str())
            }
            Self::Retiring(retiring) => match &retiring.owned {
                RetiringEnvironmentOwner::Bound(owned) => {
                    identity_is_durable(&owned.identity).then_some(owned.binding.as_str())
                }
                RetiringEnvironmentOwner::Unbound(candidate) => match &candidate.origin {
                    UnboundSessionEnvironmentOrigin::Adoption
                    | UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                        Some(candidate.binding.as_str())
                    }
                    UnboundSessionEnvironmentOrigin::New => None,
                },
            },
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => matches!(
                &candidate.origin,
                UnboundSessionEnvironmentOrigin::Adoption
                    | UnboundSessionEnvironmentOrigin::DurableAdoption(_)
            )
            .then_some(candidate.binding.as_str()),
            Self::Vacant | Self::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => None,
        }
    }
}
