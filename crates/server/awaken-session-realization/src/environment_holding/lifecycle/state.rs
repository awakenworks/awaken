// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! Pure transitions for the sole Session Environment lifecycle owner.
//!
//! Provider I/O remains in the parent module so every transition into or out
//! of these phases is performed under the same Session lifecycle guard.

use super::owner_decisions::{
    CandidateCommitDecision, DurableProjectionIdentityDecision, ProjectedEnvironmentOwner,
    candidate_receipt_decision, durable_projection_identity_decision, projected_environment_owner,
    same_durable_generation,
};
use super::{
    Arc, BoundSessionEnvironment, BoundSessionEnvironmentIdentity, RealizationError,
    RetiringEnvironmentOwner, RetiringSessionEnvironment, SessionEnvironmentOwner,
    SessionEnvironmentPreparation, SessionEnvironmentRetirementCause, UnboundSessionEnvironment,
};
use crate::environment_owner::{SessionEnvironmentRestoration, UnboundSessionEnvironmentOrigin};

pub enum RetirementSelection<'a> {
    Current,
    Exact(&'a Arc<crate::session_environment::SessionEnvironment>),
}

impl BoundSessionEnvironment {
    /// Confirm only that this original source is the one the durable projection
    /// describes. Reuse installation's exact-or-stutter identity law; this read
    /// changes no owner and grants no lease, preparation or physical effect.
    pub fn matches_source_projection(
        &self,
        state: &awaken_session_state_contract::SessionEnvironmentState,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<bool, RealizationError> {
        let ProjectedEnvironmentOwner::AwaitingAdoption { identity, binding } =
            projected_environment_owner(state, workspace_id, session_id)?
        else {
            return Ok(false);
        };
        Ok(matches!(
            durable_projection_identity_decision(
                self.identity == identity,
                self.binding == binding,
                same_durable_generation(&self.identity, &identity),
            ),
            DurableProjectionIdentityDecision::Exact
                | DurableProjectionIdentityDecision::SamePhysicalOwnerStutter
        ))
    }
}

fn compatible_durable_projection(
    owned: &BoundSessionEnvironment,
    identity: &BoundSessionEnvironmentIdentity,
    binding: &str,
) -> bool {
    durable_reservation_handles(owned, identity, binding)
        .is_some_and(|(_, projected)| owned.environment.lifecycle().handle() == projected)
}

pub fn pending_revoked_resource_reservation(
    owned: &BoundSessionEnvironment,
    identity: &BoundSessionEnvironmentIdentity,
    binding: &str,
) -> bool {
    durable_reservation_handles(owned, identity, binding).is_some_and(|(current, projected)| {
        owned
            .environment
            .lifecycle()
            .handle()
            .owned_paths_are_monotonic_to(&projected)
            && current.owned_paths() != projected.owned_paths()
    })
}

fn durable_reservation_handles(
    owned: &BoundSessionEnvironment,
    identity: &BoundSessionEnvironmentIdentity,
    binding: &str,
) -> Option<(
    awaken_provisioning_contract::SandboxHandle,
    awaken_provisioning_contract::SandboxHandle,
)> {
    let current: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(&owned.binding).ok()?;
    let projected: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(binding).ok()?;
    (same_durable_generation(&owned.identity, identity)
        && current.owned_paths_are_monotonic_to(&projected))
    .then_some((current, projected))
}

impl SessionEnvironmentOwner {
    /// Stage only the rebuildable owner coordinates. No physical owner is
    /// retired or published until the caller commits under this same installed Session lock.
    pub fn after_projection(
        &self,
        thread: &str,
        workspace_id: &str,
        state: &awaken_session_state_contract::SessionEnvironmentState,
        newer: bool,
        pending_resource_reservation: bool,
    ) -> Result<SessionEnvironmentOwner, RealizationError> {
        let projected = projected_environment_owner(state, workspace_id, thread)?;
        let mut owner = self.clone();
        owner.install_projection_with_resource_reservation(
            projected,
            newer,
            pending_resource_reservation,
        )?;
        Ok(owner)
    }

    /// Retain Store-read proof when the Session root commit succeeded but a
    /// later derivative dispatch binding was fenced. The Candidate stays
    /// hidden, retains its exact Arc/handle, and preserves Create versus Adopt
    /// retry semantics; terminal cleanup may then take over that same owner.
    pub fn retain_committed_candidate(
        &mut self,
        expected: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(RealizationError::invariant(
                "committed Session Environment Candidate lost its Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(RealizationError::invariant(
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
                return Err(RealizationError::invariant(
                    "committed Session Environment Candidate changed durable identity",
                ));
            }
        }
        Ok(current.clone())
    }

    pub fn install_projection_with_resource_reservation(
        &mut self,
        projected: ProjectedEnvironmentOwner,
        newer: bool,
        pending_resource_reservation: bool,
    ) -> Result<(), RealizationError> {
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
                if pending_resource_reservation
                    && let Self::Retiring(RetiringSessionEnvironment {
                        cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                        owned: RetiringEnvironmentOwner::Bound(owned),
                    }) = self
                    && pending_revoked_resource_reservation(owned, &identity, &binding)
                {
                    // Root Resource truth has advanced, while the revoked Arc
                    // still holds its prior physical handle. Preserve that
                    // cleanup owner until the canonical Ready/adopt edge proves
                    // and installs the exact successor. Projection alone cannot
                    // mint either a new Arc or a completed effect.
                    return Ok(());
                }
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
                    return Err(RealizationError::invariant(
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
                            return Err(RealizationError::invariant(
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
                            return Err(RealizationError::invariant(
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
                                return Err(RealizationError::invariant(
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
                                    return Err(RealizationError::invariant(
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
                            return Err(RealizationError::invariant(
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
                | Self::Retiring(_) => Err(RealizationError::invariant(
                    "restore projection conflicts with the current local Environment owner",
                )),
            },
        }
    }

    pub fn begin_preparing(
        &mut self,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        kind: awaken_session_state_contract::SessionEnvironmentEffectKind,
        intent: Option<&awaken_session_state_contract::SessionEnvironmentEffectIntent>,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        if let Some(intent) = intent {
            intent.verify().map_err(RealizationError::from)?;
            if intent.kind() != &kind {
                return Err(RealizationError::invariant(
                    "candidate changed its captured effect kind",
                ));
            }
        }
        let binding = serde_json::to_string(&environment.lifecycle().handle())
            .map_err(RealizationError::from)?;
        let origin = match (&mut *self, kind) {
            (Self::Vacant, awaken_session_state_contract::SessionEnvironmentEffectKind::Create) => {
                UnboundSessionEnvironmentOrigin::New
            }
            (
                Self::Vacant,
                awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild { .. },
            ) if intent.is_some() => UnboundSessionEnvironmentOrigin::New,
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
                // A supplied intent's kind was checked above; full equality
                // below protects it. Unknown enrollment keeps its old whitelist.
                let same_kind = intent.is_some()
                    || matches!(
                        (&existing.origin, &current_kind),
                        (
                            UnboundSessionEnvironmentOrigin::New
                                | UnboundSessionEnvironmentOrigin::DurableCreate(_),
                            awaken_session_state_contract::SessionEnvironmentEffectKind::Create
                        ) | (
                            UnboundSessionEnvironmentOrigin::Adoption
                                | UnboundSessionEnvironmentOrigin::AdoptionFrom(_)
                                | UnboundSessionEnvironmentOrigin::DurableAdoption(_),
                            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt
                        )
                    );
                if existing.intent.as_deref() != intent || !same_kind {
                    return Err(RealizationError::invariant(
                        "Session Environment candidate changed its create/adopt effect",
                    ));
                }
                return Ok(existing.clone());
            }
            _ => {
                return Err(RealizationError::invariant(
                    "Session Environment candidate conflicts with its current owner phase",
                ));
            }
        };
        let candidate = UnboundSessionEnvironment {
            origin,
            intent: intent.cloned().map(Box::new),
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
    pub fn discard_closed_pending_adoption(
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

    pub fn publish_prepared(
        &mut self,
        expected: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(RealizationError::invariant(
                "Session Environment publication lost its Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(RealizationError::invariant(
                "Session Environment publication lost its exact candidate fence",
            ));
        }
        if candidate_receipt_decision(&current.origin, &identity)
            == CandidateCommitDecision::Conflict
        {
            return Err(RealizationError::invariant(
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

    pub fn begin_restore(
        &mut self,
        request: &awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<(), RealizationError> {
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
            _ => Err(RealizationError::invariant(
                "Session Environment restore lost its exact durable phase",
            )),
        }
    }

    pub fn begin_retirement(
        &mut self,
        cause: SessionEnvironmentRetirementCause,
        selection: RetirementSelection<'_>,
    ) -> Result<Option<RetiringSessionEnvironment>, RealizationError> {
        let selected_matches =
            |environment: &Arc<crate::session_environment::SessionEnvironment>| match selection {
                RetirementSelection::Current => true,
                RetirementSelection::Exact(expected) => {
                    Arc::ptr_eq(environment, expected)
                        && environment.lifecycle().handle() == expected.lifecycle().handle()
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
                    return Err(RealizationError::invariant(
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

    pub fn begin_retirement_with_adopted(
        &mut self,
        expected_identity: &BoundSessionEnvironmentIdentity,
        expected_binding: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        cause: SessionEnvironmentRetirementCause,
    ) -> Result<RetiringSessionEnvironment, RealizationError> {
        let Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { identity, binding }) =
            self
        else {
            return Err(RealizationError::invariant(
                "adopted cleanup candidate has no durable pending owner",
            ));
        };
        let adopted_binding = serde_json::to_string(&environment.lifecycle().handle())
            .map_err(RealizationError::from)?;
        if identity != expected_identity
            || binding != expected_binding
            || adopted_binding != expected_binding
        {
            return Err(RealizationError::invariant(
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
    pub fn begin_retirement_with_committed_candidate(
        &mut self,
        expected: &UnboundSessionEnvironment,
        cause: SessionEnvironmentRetirementCause,
    ) -> Result<RetiringSessionEnvironment, RealizationError> {
        let Self::Preparing(SessionEnvironmentPreparation::Candidate(current)) = self else {
            return Err(RealizationError::invariant(
                "committed Candidate retirement has no Preparing owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(RealizationError::invariant(
                "committed Candidate retirement lost its exact local fence",
            ));
        }
        let identity = current.durable_identity().cloned().ok_or_else(|| {
            RealizationError::invariant("Candidate retirement has no Store-read durable identity")
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

    pub fn confirm_terminated(&mut self, expected: &RetiringSessionEnvironment) -> bool {
        let Self::Retiring(current) = self else {
            return false;
        };
        if !current.exact_matches(expected) {
            return false;
        }
        *self = Self::Vacant;
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn observe_retirement_status<E>(
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
    pub fn prepare_retiring_realization_adoption(
        &mut self,
        expected: &RetiringSessionEnvironment,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        intent: Option<&awaken_session_state_contract::SessionEnvironmentEffectIntent>,
    ) -> Result<(), RealizationError> {
        if let Some(intent) = intent {
            intent.verify().map_err(RealizationError::from)?;
            if !matches!(
                intent.kind(),
                awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt
            ) {
                return Err(RealizationError::invariant(
                    "re-adoption changed its captured effect kind",
                ));
            }
        }
        let Self::Retiring(current) = self else {
            return Err(RealizationError::invariant(
                "Session Environment re-adoption has no Retiring owner",
            ));
        };
        if !current.exact_matches(expected)
            || current.cause != SessionEnvironmentRetirementCause::RealizationRevocation
        {
            return Err(RealizationError::invariant(
                "Session Environment re-adoption lost its exact revocation fence",
            ));
        }
        let binding = serde_json::to_string(&environment.lifecycle().handle())
            .map_err(RealizationError::from)?;
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
                return Err(RealizationError::invariant(
                    "LegacyDirect revocation requires typed physical rebuild",
                ));
            }
        };
        let prior =
            serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(expected_binding);
        let successor = environment.lifecycle().handle();
        let physical = current.owned.environment().lifecycle().handle();
        let same_physical_successor = matches!(&current.owned,
            RetiringEnvironmentOwner::Bound(owned)
                if pending_revoked_resource_reservation(owned, &owned.identity, &binding)
                    && physical.owned_paths_are_monotonic_to(&successor));
        if !(&binding == expected_binding && successor == physical
            || same_physical_successor
                && prior.is_ok_and(|prior| prior.owned_paths_are_monotonic_to(&successor)))
        {
            return Err(RealizationError::invariant(
                "Session Environment re-adoption changed its exact physical target",
            ));
        }
        // One atomic owner replacement, not an ownerless AwaitingAdoption /
        // Vacant interval. The existing publication edge binds this Candidate
        // once and persists its authorized Adopt before exposing a Resident.
        *self = Self::Preparing(SessionEnvironmentPreparation::Candidate(
            UnboundSessionEnvironment {
                origin,
                intent: intent.cloned().map(Box::new),
                binding,
                environment,
            },
        ));
        Ok(())
    }
}
