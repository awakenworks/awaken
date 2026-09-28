// @formal surface obligation=session_environment.owner_topology_is_exact_and_frozen
// @formal reason: Defines the single process-local Environment ownership value consumed by lifecycle
// @formal reason: transitions; it adds no durable state, registry, cache, or parallel owner.
//! Strong process-local ownership state for one Session Environment.

use std::sync::Arc;

/// Exact origin of an Environment owner that predates generated durable
/// identity. Current Managed create/adopt/restore must be `Durable`, while
/// historical rows and non-Managed direct Threads remain explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LegacyDirectEnvironmentProvenance {
    Direct(awaken_session_state_contract::SessionEnvironmentReceipt),
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
        generation: awaken_session_state_contract::SandboxGeneration,
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

    pub(crate) fn effect_kind(
        &self,
    ) -> awaken_session_state_contract::SessionEnvironmentEffectKind {
        match &self.origin {
            UnboundSessionEnvironmentOrigin::New
            | UnboundSessionEnvironmentOrigin::DurableCreate(_) => {
                awaken_session_state_contract::SessionEnvironmentEffectKind::Create
            }
            UnboundSessionEnvironmentOrigin::Adoption
            | UnboundSessionEnvironmentOrigin::AdoptionFrom(_)
            | UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt
            }
        }
    }

    pub(crate) fn source_binding(&self) -> Option<&str> {
        matches!(
            self.effect_kind(),
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt
        )
        .then_some(self.binding.as_str())
    }

    /// Known source or committed identity for this exact physical owner.
    /// AdoptionFrom proves the source, not completion of the current Adopt.
    pub(crate) fn durable_identity(&self) -> Option<&BoundSessionEnvironmentIdentity> {
        match &self.origin {
            UnboundSessionEnvironmentOrigin::DurableCreate(identity)
            | UnboundSessionEnvironmentOrigin::AdoptionFrom(identity)
            | UnboundSessionEnvironmentOrigin::DurableAdoption(identity) => Some(identity),
            UnboundSessionEnvironmentOrigin::New | UnboundSessionEnvironmentOrigin::Adoption => {
                None
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
// These variants are the exact in-memory ownership proof retained across a
// fallible adoption. There is one per Session slot; heap-indirecting only the
// durable identity would obscure its value semantics without changing wire or
// persistence size.
#[allow(clippy::large_enum_variant)]
pub(crate) enum UnboundSessionEnvironmentOrigin {
    New,
    Adoption,
    /// A durable source is being adopted; its receipt predates this Adopt.
    /// Preserve it for revocation/cleanup until the current receipt is read.
    AdoptionFrom(BoundSessionEnvironmentIdentity),
    /// The Session root committed the Create receipt, but a later derivative
    /// dispatch-cache write was fenced before the exact Arc could be published.
    /// This remains a hidden Candidate and preserves Create retry semantics.
    DurableCreate(BoundSessionEnvironmentIdentity),
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

    pub(crate) fn activity_generation_id(&self) -> String {
        match &self.identity {
            BoundSessionEnvironmentIdentity::Durable { generation, .. } => generation.id.clone(),
            BoundSessionEnvironmentIdentity::LegacyDirect(_) => {
                self.environment.handle().sandbox_id
            }
        }
    }
}

#[derive(Clone)]
// AwaitingAdoption deliberately retains the complete durable identity while
// Candidate retains the exact Arc. Both are mutually exclusive phases of the
// single Session slot owner, not elements of a retained collection.
#[allow(clippy::large_enum_variant)]
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
        request: awaken_session_state_contract::SandboxRestoreRequest,
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
        operation: awaken_session_state_contract::SessionEnvironmentOperation,
        generation: awaken_session_state_contract::SandboxGeneration,
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
// Retirement must carry either the exact unpublished Candidate or the exact
// published owner across cancellation and response loss. The enum is a single
// slot phase, so preserving direct value ownership is preferable to a second
// heap-owned identity wrapper.
#[allow(clippy::large_enum_variant)]
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
    /// Exact physical binding descriptor, including pending adoption and an
    /// unacknowledged Candidate. This asserts neither local realization nor
    /// root commitment; durable_binding owns the latter distinction.
    pub(crate) fn physical_binding(&self) -> Option<&str> {
        match self {
            Self::Vacant | Self::Restoring(_) => None,
            Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                binding, ..
            }) => Some(binding),
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => {
                Some(&candidate.binding)
            }
            Self::Resident(owned) => Some(&owned.binding),
            Self::Retiring(retiring) => Some(retiring.owned.binding()),
        }
    }

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

    /// Execution may prepare the enrolled candidate or use the published owner,
    /// never a detached/retiring owner retained only for terminal cleanup.
    #[cfg(target_os = "linux")]
    pub(crate) fn execution_environment(
        &self,
    ) -> Option<&Arc<crate::session_environment::SessionEnvironment>> {
        match self {
            Self::Resident(owned) => Some(&owned.environment),
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => {
                Some(&candidate.environment)
            }
            _ => None,
        }
    }

    /// Project the single activity-generation key for this exact Resident Arc.
    /// Managed durable Sessions use the aggregate's Sandbox generation; legacy
    /// direct Sessions retain their existing physical-sandbox key. Checking both
    /// Arc identity and the evidence-bearing handle prevents a stale wrapper from
    /// borrowing another Resident owner's background/quiescence domain.
    pub(crate) fn resident_activity_generation_id(
        &self,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Option<String> {
        let Self::Resident(owned) = self else {
            return None;
        };
        if !Arc::ptr_eq(&owned.environment, environment)
            || owned.environment.handle() != environment.handle()
        {
            return None;
        }
        Some(owned.activity_generation_id())
    }

    /// Snapshot the exact published Environment owner used by terminal cleanup.
    /// A retryable Bound retirement remains physical authority even though
    /// ordinary tool readers must no longer see it as Resident. Unbound
    /// candidates and pending/restoring projections have never published a
    /// Runtime background domain and are intentionally excluded.
    pub(crate) fn terminal_bound_environment(&self) -> Option<BoundSessionEnvironment> {
        let owned = match self {
            Self::Resident(owned) => owned,
            Self::Retiring(RetiringSessionEnvironment {
                owned: RetiringEnvironmentOwner::Bound(owned),
                ..
            }) => owned,
            Self::Vacant
            | Self::Preparing(_)
            | Self::Restoring(_)
            | Self::Retiring(RetiringSessionEnvironment {
                owned: RetiringEnvironmentOwner::Unbound(_),
                ..
            }) => return None,
        };
        Some(owned.clone())
    }

    pub(crate) fn has_local_environment(&self) -> bool {
        matches!(
            self,
            Self::Preparing(SessionEnvironmentPreparation::Candidate(_))
                | Self::Resident(_)
                | Self::Retiring(_)
        )
    }

    /// A Store response may be lost after the durable binding commits but
    /// before this process receives the generated identity. The one hidden
    /// Candidate must remain eligible for its existing idempotent publication
    /// retry; it is not a second Resident or a license to create a substitute.
    pub(crate) fn unpublished_candidate(&self) -> Option<&UnboundSessionEnvironment> {
        match self {
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => Some(candidate),
            Self::Vacant
            | Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
            | Self::Restoring(_)
            | Self::Resident(_)
            | Self::Retiring(_) => None,
        }
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
                    UnboundSessionEnvironmentOrigin::DurableCreate(_)
                    | UnboundSessionEnvironmentOrigin::AdoptionFrom(_)
                    | UnboundSessionEnvironmentOrigin::Adoption
                    | UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                        Some(candidate.binding.as_str())
                    }
                    UnboundSessionEnvironmentOrigin::New => None,
                },
            },
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => matches!(
                &candidate.origin,
                UnboundSessionEnvironmentOrigin::DurableCreate(_)
                    | UnboundSessionEnvironmentOrigin::AdoptionFrom(_)
                    | UnboundSessionEnvironmentOrigin::Adoption
                    | UnboundSessionEnvironmentOrigin::DurableAdoption(_)
            )
            .then_some(candidate.binding.as_str()),
            Self::Vacant | Self::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => None,
        }
    }
}
