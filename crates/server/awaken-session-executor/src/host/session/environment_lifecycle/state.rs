// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! Pure transitions for the sole Session Environment lifecycle owner.
//!
//! Provider I/O remains in the parent module so every transition into or out
//! of these phases is performed under the same Session lifecycle guard.

use super::*;
use crate::session_slot::{
    LegacyEnvironmentEffect, SessionEnvironmentRestoration, UnboundSessionEnvironmentOrigin,
};

pub(super) enum ProjectedEnvironmentOwner {
    Vacant,
    AwaitingAdoption {
        identity: BoundSessionEnvironmentIdentity,
        binding: String,
    },
    Restoring {
        request: awaken_session_state_contract::SandboxRestoreRequest,
    },
}

pub(super) enum RetirementSelection<'a> {
    Current,
    Exact(&'a Arc<crate::session_environment::SessionEnvironment>),
}

fn legacy_effect(effect_id: &Option<String>) -> LegacyEnvironmentEffect {
    match effect_id {
        Some(effect_id) => LegacyEnvironmentEffect::Known(effect_id.clone()),
        None => LegacyEnvironmentEffect::Absent,
    }
}

pub(super) fn checkpoint_source_identity(
    source_effect_id: &str,
    source_binding: &str,
    generation: &awaken_session_state_contract::SandboxGeneration,
) -> BoundSessionEnvironmentIdentity {
    if source_effect_id.is_empty() {
        BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::DurableBinding {
                binding: source_binding.to_string(),
                effect: LegacyEnvironmentEffect::Absent,
            },
        )
    } else {
        BoundSessionEnvironmentIdentity::Durable {
            effect_id: source_effect_id.to_string(),
            generation: generation.clone(),
        }
    }
}

pub(super) fn projected_environment_owner(
    state: &awaken_session_state_contract::SessionEnvironmentState,
    workspace_id: &str,
    session_id: &str,
) -> Result<ProjectedEnvironmentOwner, HostError> {
    use awaken_session_state_contract::SessionEnvironmentState;
    match state {
        SessionEnvironmentState::Unmaterialized | SessionEnvironmentState::Hibernated { .. } => {
            Ok(ProjectedEnvironmentOwner::Vacant)
        }
        SessionEnvironmentState::Resident {
            binding,
            effect_id,
            generation,
            ..
        } => {
            let identity = match (effect_id, generation) {
                (Some(effect_id), Some(generation)) => BoundSessionEnvironmentIdentity::Durable {
                    effect_id: effect_id.clone(),
                    generation: generation.clone(),
                },
                (effect_id, None) => BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding: binding.clone(),
                        effect: legacy_effect(effect_id),
                    },
                ),
                (None, Some(_)) => {
                    return Err(HostError::internal(
                        "durable Session Environment generation has no effect identity",
                    ));
                }
            };
            Ok(ProjectedEnvironmentOwner::AwaitingAdoption {
                identity,
                binding: binding.clone(),
            })
        }
        SessionEnvironmentState::Suspending {
            source_effect_id,
            source_binding,
            generation,
            ..
        } => Ok(ProjectedEnvironmentOwner::AwaitingAdoption {
            identity: checkpoint_source_identity(source_effect_id, source_binding, generation),
            binding: source_binding.clone(),
        }),
        SessionEnvironmentState::Restoring { .. } => Ok(ProjectedEnvironmentOwner::Restoring {
            request: state
                .restoring_request(
                    workspace_id,
                    &awaken_session_types::SessionId::from(session_id),
                )
                .expect("Restoring state projects one exact restore request"),
        }),
    }
}

fn compatible_durable_projection(
    owned: &BoundSessionEnvironment,
    identity: &BoundSessionEnvironmentIdentity,
    binding: &str,
) -> bool {
    let current =
        serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(&owned.binding);
    let projected = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(binding);
    let same_generation = same_durable_generation(&owned.identity, identity);
    matches!((current, projected), (Ok(current), Ok(projected))
        if same_generation
            && current.owned_paths_are_monotonic_to(&projected)
            && owned.environment.handle() == projected)
}

fn same_durable_generation(
    current: &BoundSessionEnvironmentIdentity,
    projected: &BoundSessionEnvironmentIdentity,
) -> bool {
    matches!(
        (current, projected),
        (
            BoundSessionEnvironmentIdentity::Durable {
                generation: current,
                ..
            },
            BoundSessionEnvironmentIdentity::Durable {
                generation: projected,
                ..
            },
        ) if current == projected
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableProjectionIdentityDecision {
    Exact,
    SamePhysicalOwnerStutter,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateCommitClass {
    New,
    Adoption,
    AdoptionFrom,
    DurableCreate,
    DurableAdoption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateCommitDecision {
    BindCreate,
    BindAdoption,
    Replay,
    Conflict,
}

const fn candidate_commit_decision(
    class: CandidateCommitClass,
    committed_identity_is_exact: bool,
    source_identity_is_compatible: bool,
) -> CandidateCommitDecision {
    match class {
        CandidateCommitClass::New => CandidateCommitDecision::BindCreate,
        CandidateCommitClass::Adoption => CandidateCommitDecision::BindAdoption,
        CandidateCommitClass::AdoptionFrom if source_identity_is_compatible => {
            CandidateCommitDecision::BindAdoption
        }
        CandidateCommitClass::AdoptionFrom => CandidateCommitDecision::Conflict,
        CandidateCommitClass::DurableCreate | CandidateCommitClass::DurableAdoption
            if committed_identity_is_exact =>
        {
            CandidateCommitDecision::Replay
        }
        CandidateCommitClass::DurableCreate | CandidateCommitClass::DurableAdoption => {
            CandidateCommitDecision::Conflict
        }
    }
}

// Source receipts and the current effect receipt have different roles. Both
// publication and post-commit retention use this decision; otherwise dispatch
// success could accept a receipt that the failure/retry path rejects.
fn candidate_receipt_decision(
    origin: &UnboundSessionEnvironmentOrigin,
    identity: &BoundSessionEnvironmentIdentity,
) -> CandidateCommitDecision {
    let (class, exact, compatible) = match origin {
        UnboundSessionEnvironmentOrigin::New => (CandidateCommitClass::New, false, false),
        UnboundSessionEnvironmentOrigin::Adoption => (CandidateCommitClass::Adoption, false, false),
        UnboundSessionEnvironmentOrigin::AdoptionFrom(source) => (
            CandidateCommitClass::AdoptionFrom,
            false,
            source == identity
                || same_durable_generation(source, identity)
                || matches!(
                    (source, identity),
                    (
                        BoundSessionEnvironmentIdentity::LegacyDirect(
                            LegacyDirectEnvironmentProvenance::DurableBinding { .. }
                        ),
                        BoundSessionEnvironmentIdentity::Durable { .. }
                    )
                ),
        ),
        UnboundSessionEnvironmentOrigin::DurableCreate(current) => (
            CandidateCommitClass::DurableCreate,
            current == identity,
            false,
        ),
        UnboundSessionEnvironmentOrigin::DurableAdoption(current) => (
            CandidateCommitClass::DurableAdoption,
            current == identity,
            false,
        ),
    };
    candidate_commit_decision(class, exact, compatible)
}

const fn durable_projection_identity_decision(
    identity_is_exact: bool,
    binding_is_exact: bool,
    generation_is_exact: bool,
) -> DurableProjectionIdentityDecision {
    if identity_is_exact && binding_is_exact {
        DurableProjectionIdentityDecision::Exact
    } else if !identity_is_exact && binding_is_exact && generation_is_exact {
        DurableProjectionIdentityDecision::SamePhysicalOwnerStutter
    } else {
        DurableProjectionIdentityDecision::Conflict
    }
}

impl SessionEnvironmentOwner {
    /// Retain Store-read proof when the Session root commit succeeded but a
    /// later derivative dispatch binding was fenced. The Candidate stays
    /// hidden, retains its exact Arc/handle, and preserves Create versus Adopt
    /// retry semantics; terminal cleanup may then take over that same owner.
    pub(super) fn retain_committed_candidate(
        &mut self,
        expected: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(HostError::internal(
                "committed Session Environment Candidate lost its Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(HostError::internal(
                "committed Session Environment Candidate lost its exact fence",
            ));
        }
        match candidate_receipt_decision(&current.origin, &identity) {
            CandidateCommitDecision::BindCreate => {
                current.origin = UnboundSessionEnvironmentOrigin::DurableCreate(identity);
            }
            CandidateCommitDecision::BindAdoption => {
                current.origin = UnboundSessionEnvironmentOrigin::DurableAdoption(identity);
            }
            CandidateCommitDecision::Replay => {}
            CandidateCommitDecision::Conflict => {
                return Err(HostError::internal(
                    "committed Session Environment Candidate changed durable identity",
                ));
            }
        }
        Ok(current.clone())
    }

    pub(super) fn install_projection(
        &mut self,
        projected: ProjectedEnvironmentOwner,
        newer: bool,
    ) -> Result<(), HostError> {
        // A pending adoption is only a cached descriptor, never a physical Arc.
        // A newer complete root snapshot may replace it; all actual owners still
        // pass through the exact observation/retirement protocol below.
        if newer
            && matches!(
                self,
                Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
            )
        {
            *self = Self::Vacant;
        }
        match projected {
            ProjectedEnvironmentOwner::Vacant => match self {
                Self::Vacant | Self::Retiring(_) => Ok(()),
                Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
                | Self::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => {
                    *self = Self::Vacant;
                    Ok(())
                }
                // A FrozenSessionProjection may be read before this process
                // commits the create/adopt receipt, then wait on the lifecycle
                // guard while that receipt publishes the exact Candidate. Its
                // Unmaterialized value is therefore older than the local
                // Candidate/Resident owner and cannot unbind or reject it. A
                // real durable retirement first moves the local owner through
                // Retiring, so retaining these phases does not hide cleanup.
                Self::Preparing(SessionEnvironmentPreparation::Candidate(_))
                | Self::Resident(_) => Ok(()),
            },
            ProjectedEnvironmentOwner::AwaitingAdoption { identity, binding } => {
                if let Self::Resident(owned) = self
                    && (owned.identity != identity || owned.binding != binding)
                {
                    // A live Resource reservation is the sole durable update
                    // that can change a Resident handle without replacing its
                    // substrate. The aggregate has already admitted the receipt;
                    // this owner additionally requires the same durable
                    // generation, the provisioning contract's canonical
                    // monotonic-handle proof, and the exact handle currently
                    // emitted by its retained Arc. No projected string alone can
                    // rebind a process-local owner.
                    if owned.binding != binding
                        && compatible_durable_projection(owned, &identity, &binding)
                    {
                        owned.identity = identity;
                        owned.binding = binding;
                        return Ok(());
                    }
                }
                if let Self::Retiring(RetiringSessionEnvironment {
                    cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                    owned: RetiringEnvironmentOwner::Bound(owned),
                }) = self
                    && (owned.identity != identity || owned.binding != binding)
                    && owned.binding != binding
                    && compatible_durable_projection(owned, &identity, &binding)
                {
                    // Revocation has already closed every local execution path,
                    // and the lifecycle guard excludes a concurrent retirement
                    // effect. Advance only the durable receipt identity for the
                    // same physical generation/Arc so an interrupted Resource
                    // reservation cannot strand terminal cleanup behind its
                    // superseded receipt. Other Retiring causes keep their exact
                    // operation fence and remain strict below.
                    owned.identity = identity;
                    owned.binding = binding;
                    return Ok(());
                }
                if let Some(existing) = self.physical_binding()
                    && existing != binding
                {
                    return Err(HostError::internal(
                        "durable Session Environment projection changed its physical binding",
                    ));
                }
                match self {
                    Self::Vacant => {
                        *self = Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                            identity,
                            binding,
                        });
                    }
                    Self::Restoring(SessionEnvironmentRestoration::Awaiting { request }) => {
                        let exact_restore = matches!(
                            &identity,
                            BoundSessionEnvironmentIdentity::Durable {
                                effect_id,
                                generation,
                            } if effect_id == &request.effect_id
                                && generation.id() == request.generation_id
                        );
                        if !exact_restore {
                            return Err(HostError::internal(
                                "Resident restore projection does not match its exact durable request",
                            ));
                        }
                        *self = Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                            identity,
                            binding,
                        });
                    }
                    Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                        identity: current,
                        ..
                    }) => *current = identity,
                    // A projection can confirm durable truth but cannot publish
                    // a physical candidate. Only the Store-read sink receipt
                    // consumed by `publish_prepared` crosses that boundary.
                    Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => {
                        if let Some(committed) = candidate.durable_identity()
                            && durable_projection_identity_decision(
                                committed == &identity,
                                candidate.binding == binding,
                                same_durable_generation(committed, &identity),
                            ) == DurableProjectionIdentityDecision::Conflict
                        {
                            return Err(HostError::internal(
                                "committed Candidate projection changed its durable identity",
                            ));
                        }
                    }
                    Self::Resident(owned) => {
                        match durable_projection_identity_decision(
                            owned.identity == identity,
                            owned.binding == binding,
                            same_durable_generation(&owned.identity, &identity),
                        ) {
                            DurableProjectionIdentityDecision::Exact
                            | DurableProjectionIdentityDecision::SamePhysicalOwnerStutter => {}
                            DurableProjectionIdentityDecision::Conflict => {
                                return Err(HostError::internal(
                                    "resident Session Environment projection changed its durable identity",
                                ));
                            }
                        }
                    }
                    Self::Retiring(retiring) => match &retiring.owned {
                        RetiringEnvironmentOwner::Bound(owned) => {
                            match durable_projection_identity_decision(
                                owned.identity == identity,
                                owned.binding == binding,
                                same_durable_generation(&owned.identity, &identity),
                            ) {
                                DurableProjectionIdentityDecision::Exact
                                | DurableProjectionIdentityDecision::SamePhysicalOwnerStutter => {}
                                DurableProjectionIdentityDecision::Conflict => {
                                    return Err(HostError::internal(
                                        "retiring Session Environment projection changed its durable identity",
                                    ));
                                }
                            }
                        }
                        RetiringEnvironmentOwner::Unbound(_)
                            if retiring.cause
                                == SessionEnvironmentRetirementCause::RealizationRevocation =>
                        {
                            // The exact binding check above confirms only the
                            // physical target. Keep it Unbound: current Adopt
                            // sink readback, not projection, supplies identity.
                        }
                        RetiringEnvironmentOwner::Unbound(_) => {
                            return Err(HostError::internal(
                                "durable projection cannot promote an unpublished retirement",
                            ));
                        }
                    },
                }
                Ok(())
            }
            ProjectedEnvironmentOwner::Restoring { request } => match self {
                Self::Vacant
                | Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. }) => {
                    *self = Self::Restoring(SessionEnvironmentRestoration::Awaiting { request });
                    Ok(())
                }
                Self::Restoring(SessionEnvironmentRestoration::Awaiting { request: current })
                    if *current == request =>
                {
                    Ok(())
                }
                Self::Preparing(SessionEnvironmentPreparation::Candidate(_))
                | Self::Restoring(_)
                | Self::Resident(_)
                | Self::Retiring(_) => Err(HostError::internal(
                    "restore projection conflicts with the current local Environment owner",
                )),
            },
        }
    }

    pub(super) fn begin_preparing(
        &mut self,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        kind: awaken_session_state_contract::SessionEnvironmentEffectKind,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        let binding = serde_json::to_string(&environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let origin = match (&mut *self, kind) {
            (Self::Vacant, awaken_session_state_contract::SessionEnvironmentEffectKind::Create) => {
                UnboundSessionEnvironmentOrigin::New
            }
            (Self::Vacant, awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt) => {
                UnboundSessionEnvironmentOrigin::Adoption
            }
            (
                Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                    identity,
                    binding: expected,
                }),
                awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
            ) if *expected == binding => {
                UnboundSessionEnvironmentOrigin::AdoptionFrom(identity.clone())
            }
            (Self::Preparing(SessionEnvironmentPreparation::Candidate(existing)), current_kind)
                if existing.binding == binding
                    && Arc::ptr_eq(&existing.environment, &environment) =>
            {
                if existing.effect_kind() != current_kind {
                    return Err(HostError::internal(
                        "Session Environment candidate changed its create/adopt effect",
                    ));
                }
                return Ok(existing.clone());
            }
            _ => {
                return Err(HostError::internal(
                    "Session Environment candidate conflicts with its current owner phase",
                ));
            }
        };
        let candidate = UnboundSessionEnvironment {
            origin,
            binding,
            environment,
        };
        *self = Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate.clone()));
        Ok(candidate)
    }

    /// Consume a cold pending owner only after the provider has proved that its
    /// exact durable handle is closed. The caller holds the lifecycle lock and
    /// supplies the identity captured before observation, so a same-binding ABA
    /// projection cannot be cleared by stale evidence.
    pub(super) fn discard_closed_pending_adoption(
        &mut self,
        expected_identity: &BoundSessionEnvironmentIdentity,
        expected_binding: &str,
    ) -> bool {
        let Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { identity, binding }) =
            self
        else {
            return false;
        };
        if identity != expected_identity || binding != expected_binding {
            return false;
        }
        *self = Self::Vacant;
        true
    }

    pub(super) fn publish_prepared(
        &mut self,
        expected: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(HostError::internal(
                "Session Environment publication lost its Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(HostError::internal(
                "Session Environment publication lost its exact candidate fence",
            ));
        }
        if candidate_receipt_decision(&current.origin, &identity)
            == CandidateCommitDecision::Conflict
        {
            return Err(HostError::internal(
                "Session Environment publication changed durable identity",
            ));
        }
        let resident = BoundSessionEnvironment {
            identity,
            binding: current.binding.clone(),
            environment: current.environment.clone(),
        };
        let environment = resident.environment.clone();
        *self = Self::Resident(resident);
        Ok(environment)
    }

    pub(super) fn begin_restore(
        &mut self,
        request: &awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<(), HostError> {
        match self {
            Self::Vacant => {
                *self = Self::Restoring(SessionEnvironmentRestoration::Awaiting {
                    request: request.clone(),
                });
                Ok(())
            }
            Self::Restoring(SessionEnvironmentRestoration::Awaiting { request: current })
                if current == request =>
            {
                Ok(())
            }
            _ => Err(HostError::internal(
                "Session Environment restore lost its exact durable phase",
            )),
        }
    }

    pub(super) fn begin_retirement(
        &mut self,
        cause: SessionEnvironmentRetirementCause,
        selection: RetirementSelection<'_>,
    ) -> Result<Option<RetiringSessionEnvironment>, HostError> {
        let selected_matches =
            |environment: &Arc<crate::session_environment::SessionEnvironment>| match selection {
                RetirementSelection::Current => true,
                RetirementSelection::Exact(expected) => {
                    Arc::ptr_eq(environment, expected) && environment.handle() == expected.handle()
                }
            };
        let owned = match self {
            Self::Vacant
            | Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
            | Self::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => return Ok(None),
            Self::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => {
                if !selected_matches(&candidate.environment) {
                    return Ok(None);
                }
                if cause == SessionEnvironmentRetirementCause::RealizationRevocation
                    && candidate.durable_identity().is_some()
                {
                    // Root readback, not projected binding equality, permits
                    // the existing Bound retirement/adoption path. Preserve
                    // uncommitted Candidates as Unbound cleanup owners.
                    let candidate = candidate.clone();
                    return self
                        .begin_retirement_with_committed_candidate(&candidate, cause)
                        .map(Some);
                }
                RetiringEnvironmentOwner::Unbound(candidate.clone())
            }
            Self::Resident(owned) => {
                if !selected_matches(&owned.environment) {
                    return Ok(None);
                }
                RetiringEnvironmentOwner::Bound(owned.clone())
            }
            Self::Retiring(retiring) => {
                if !selected_matches(&retiring.owned.environment()) {
                    return Ok(None);
                }
                if retiring.cause == cause {
                    return Ok(Some(retiring.clone()));
                }
                let terminal_takeover = matches!(
                    (&retiring.cause, &cause),
                    (
                        SessionEnvironmentRetirementCause::UnpublishedCandidate
                            | SessionEnvironmentRetirementCause::RecoveryDiscard
                            | SessionEnvironmentRetirementCause::RealizationRevocation
                            | SessionEnvironmentRetirementCause::CheckpointSource { .. },
                        SessionEnvironmentRetirementCause::Terminal { .. }
                    )
                );
                if !terminal_takeover {
                    return Err(HostError::internal(
                        "lower-priority Session Environment retirement cannot replace its in-flight cause",
                    ));
                }
                retiring.owned.clone()
            }
        };
        let retiring = RetiringSessionEnvironment { cause, owned };
        *self = Self::Retiring(retiring.clone());
        Ok(Some(retiring))
    }

    pub(super) fn begin_retirement_with_adopted(
        &mut self,
        expected_identity: &BoundSessionEnvironmentIdentity,
        expected_binding: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        cause: SessionEnvironmentRetirementCause,
    ) -> Result<RetiringSessionEnvironment, HostError> {
        let Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { identity, binding }) =
            self
        else {
            return Err(HostError::internal(
                "adopted cleanup candidate has no durable pending owner",
            ));
        };
        let adopted_binding = serde_json::to_string(&environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        if identity != expected_identity
            || binding != expected_binding
            || adopted_binding != expected_binding
        {
            return Err(HostError::internal(
                "adopted cleanup candidate lost its exact durable fence",
            ));
        }
        let retiring = RetiringSessionEnvironment {
            cause,
            owned: RetiringEnvironmentOwner::Bound(BoundSessionEnvironment {
                identity: identity.clone(),
                binding: binding.clone(),
                environment,
            }),
        };
        *self = Self::Retiring(retiring.clone());
        Ok(retiring)
    }

    /// Move a root-committed but locally unpublished Candidate directly behind
    /// a retirement fence. It never becomes Resident/tool-visible; the durable
    /// identity and exact Arc become one Bound retirement owner so preparation
    /// and disposal/revocation retries share their ordinary lifecycle paths.
    pub(super) fn begin_retirement_with_committed_candidate(
        &mut self,
        expected: &UnboundSessionEnvironment,
        cause: SessionEnvironmentRetirementCause,
    ) -> Result<RetiringSessionEnvironment, HostError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(HostError::internal(
                "committed Candidate retirement has no Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(HostError::internal(
                "committed Candidate retirement lost its exact local fence",
            ));
        }
        let identity = current.durable_identity().cloned().ok_or_else(|| {
            HostError::internal("Candidate retirement has no Store-read durable identity")
        })?;
        let retiring = RetiringSessionEnvironment {
            cause,
            owned: RetiringEnvironmentOwner::Bound(BoundSessionEnvironment {
                identity,
                binding: current.binding.clone(),
                environment: current.environment.clone(),
            }),
        };
        *self = Self::Retiring(retiring.clone());
        Ok(retiring)
    }

    pub(super) fn confirm_terminated(&mut self, expected: &RetiringSessionEnvironment) -> bool {
        let Self::Retiring(current) = self else {
            return false;
        };
        if !current.exact_matches(expected) {
            return false;
        }
        *self = Self::Vacant;
        true
    }

    #[cfg(test)]
    pub(super) fn observe_retirement_status<E>(
        &mut self,
        expected: &RetiringSessionEnvironment,
        status: Result<awaken_provisioning_contract::SandboxStatus, E>,
    ) -> Result<bool, E> {
        match status {
            Ok(awaken_provisioning_contract::SandboxStatus::Terminated) => {
                Ok(self.confirm_terminated(expected))
            }
            Ok(
                awaken_provisioning_contract::SandboxStatus::Provisioning
                | awaken_provisioning_contract::SandboxStatus::Ready,
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }
    /// Enroll the fresh wrapper for an exact owner revoked by realization loss.
    /// The physical Sandbox remains untouched. Preserve known durable identity,
    /// but never invent one for an unacknowledged Candidate. LegacyDirect
    /// retirement is owned by the separate typed physical-rebuild path.
    pub(super) fn prepare_retiring_realization_adoption(
        &mut self,
        expected: &RetiringSessionEnvironment,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(), HostError> {
        let Self::Retiring(current) = self else {
            return Err(HostError::internal(
                "Session Environment re-adoption has no Retiring owner",
            ));
        };
        if !current.exact_matches(expected)
            || current.cause != SessionEnvironmentRetirementCause::RealizationRevocation
        {
            return Err(HostError::internal(
                "Session Environment re-adoption lost its exact revocation fence",
            ));
        }
        let binding = serde_json::to_string(&environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let (expected_binding, origin) = match &current.owned {
            RetiringEnvironmentOwner::Bound(owned)
                if matches!(
                    &owned.identity,
                    BoundSessionEnvironmentIdentity::Durable { .. }
                ) =>
            {
                (
                    &owned.binding,
                    UnboundSessionEnvironmentOrigin::AdoptionFrom(owned.identity.clone()),
                )
            }
            RetiringEnvironmentOwner::Unbound(candidate) => (
                &candidate.binding,
                UnboundSessionEnvironmentOrigin::Adoption,
            ),
            _ => {
                return Err(HostError::internal(
                    "LegacyDirect revocation requires typed physical rebuild",
                ));
            }
        };
        if &binding != expected_binding
            || environment.handle() != current.owned.environment().handle()
        {
            return Err(HostError::internal(
                "Session Environment re-adoption changed its exact physical target",
            ));
        }
        // One atomic owner replacement, not an ownerless AwaitingAdoption /
        // Vacant interval. The existing publication edge binds this Candidate
        // once and persists its authorized Adopt before exposing a Resident.
        *self = Self::Preparing(SessionEnvironmentPreparation::Candidate(
            UnboundSessionEnvironment {
                origin,
                binding,
                environment,
            },
        ));
        Ok(())
    }
}

#[cfg(kani)]
/// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
/// @proof-input-debt free_symbol durable_projection_identity_decision binding_is_exact
/// @proof-input-debt free_symbol durable_projection_identity_decision identity_is_exact
#[kani::proof]
fn durable_projection_identity_is_exact_or_a_same_physical_owner_stutter() {
    // Cause/effect decision table:
    // C1 effect identity is exact; C2 binding is exact; C3 immutable generation
    // is exact. E1 C1+C2 => Exact. E2 !C1+C2+C3 => stale same-owner stutter.
    // E3 every remaining combination => Conflict. The eight Boolean rules are
    // exhausted here; only E1/E2 may be absorbed by Resident or Retiring.
    let identity_is_exact: bool = kani::any();
    let binding_is_exact: bool = kani::any();
    let generation_is_exact: bool = kani::any();
    let decision = durable_projection_identity_decision(
        identity_is_exact,
        binding_is_exact,
        generation_is_exact,
    );
    assert_eq!(
        decision == DurableProjectionIdentityDecision::Exact,
        identity_is_exact && binding_is_exact
    );
    assert_eq!(
        decision == DurableProjectionIdentityDecision::SamePhysicalOwnerStutter,
        !identity_is_exact && binding_is_exact && generation_is_exact
    );
}

#[cfg(kani)]
/// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
#[kani::proof]
fn committed_candidate_retains_effect_kind_and_rejects_identity_relabeling() {
    // Cause/effect decision table: C1 is the closed Candidate origin class; C2
    // a previously committed identity is exact. E1 New binds DurableCreate;
    // E2 Adoption binds DurableAdoption; source adoption binds only a compatible
    // physical generation (or typed legacy upgrade). E3 either committed class plus exact C2
    // replays; E4 either durable class plus !C2 conflicts. The reducer returns
    // no Resident decision, so every row remains hidden until the canonical
    // publication or terminal-retirement owner consumes it.
    let class = match kani::any::<u8>() % 5 {
        0 => CandidateCommitClass::New,
        1 => CandidateCommitClass::Adoption,
        2 => CandidateCommitClass::DurableCreate,
        3 => CandidateCommitClass::DurableAdoption,
        _ => CandidateCommitClass::AdoptionFrom,
    };
    let exact: bool = kani::any();
    let compatible: bool = kani::any();
    let decision = candidate_commit_decision(class, exact, compatible);
    match class {
        CandidateCommitClass::New => assert_eq!(decision, CandidateCommitDecision::BindCreate),
        CandidateCommitClass::Adoption => {
            assert_eq!(decision, CandidateCommitDecision::BindAdoption)
        }
        CandidateCommitClass::AdoptionFrom => assert_eq!(
            decision,
            if compatible {
                CandidateCommitDecision::BindAdoption
            } else {
                CandidateCommitDecision::Conflict
            }
        ),
        CandidateCommitClass::DurableCreate | CandidateCommitClass::DurableAdoption if exact => {
            assert_eq!(decision, CandidateCommitDecision::Replay)
        }
        CandidateCommitClass::DurableCreate | CandidateCommitClass::DurableAdoption => {
            assert_eq!(decision, CandidateCommitDecision::Conflict)
        }
    }
}

pub(in crate::host::session) fn committed_identity(
    receipt: &awaken_session_state_contract::SessionEnvironmentReceipt,
    committed: awaken_session_state_contract::SessionEnvironmentState,
) -> Result<BoundSessionEnvironmentIdentity, HostError> {
    let awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: Some(effect_id),
        generation: Some(generation),
        ..
    } = committed
    else {
        return Err(HostError::internal(
            "durable Environment binding sink returned no generated Resident authority",
        ));
    };
    if binding != receipt.binding || effect_id != receipt.effect_id {
        return Err(HostError::internal(
            "durable Environment binding sink returned mismatched committed authority",
        ));
    }
    Ok(BoundSessionEnvironmentIdentity::Durable {
        effect_id,
        generation,
    })
}
