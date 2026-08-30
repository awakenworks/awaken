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
        request: awaken_session_contract::SandboxRestoreRequest,
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
    generation: &awaken_session_contract::SandboxGeneration,
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
    state: &awaken_session_contract::SessionEnvironmentState,
    workspace_id: &str,
    session_id: &str,
) -> Result<ProjectedEnvironmentOwner, HostError> {
    use awaken_session_contract::SessionEnvironmentState;
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
                .restoring_request(workspace_id, session_id)
                .expect("Restoring state projects one exact restore request"),
        }),
    }
}

fn owned_binding(owner: &SessionEnvironmentOwner) -> Option<&str> {
    match owner {
        SessionEnvironmentOwner::Vacant
        | SessionEnvironmentOwner::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => {
            None
        }
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
            binding,
            ..
        }) => Some(binding),
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(candidate)) => {
            Some(&candidate.binding)
        }
        SessionEnvironmentOwner::Resident(owned) => Some(&owned.binding),
        SessionEnvironmentOwner::Retiring(retiring) => Some(retiring.owned.binding()),
    }
}

impl SessionEnvironmentOwner {
    pub(super) fn seed_pending_adoption(
        &mut self,
        identity: BoundSessionEnvironmentIdentity,
        binding: String,
    ) -> Result<(), HostError> {
        match self {
            Self::Vacant => {
                *self = Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                    identity,
                    binding,
                });
                Ok(())
            }
            Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                identity: current_identity,
                binding: current_binding,
            }) if current_identity == &identity && current_binding == &binding => Ok(()),
            _ => Err(HostError::internal(
                "pending Session Environment adoption conflicts with its current owner",
            )),
        }
    }

    pub(super) fn install_projection(
        &mut self,
        projected: ProjectedEnvironmentOwner,
    ) -> Result<(), HostError> {
        match projected {
            ProjectedEnvironmentOwner::Vacant => match self {
                Self::Vacant | Self::Retiring(_) => Ok(()),
                Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
                | Self::Restoring(SessionEnvironmentRestoration::Awaiting { .. }) => {
                    *self = Self::Vacant;
                    Ok(())
                }
                Self::Preparing(SessionEnvironmentPreparation::Candidate(_))
                | Self::Resident(_) => Err(HostError::internal(
                    "durable Session Environment became unbound while a local owner remained live",
                )),
            },
            ProjectedEnvironmentOwner::AwaitingAdoption { identity, binding } => {
                if let Some(existing) = owned_binding(self)
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
                                && generation.id == request.generation_id
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
                    Self::Preparing(SessionEnvironmentPreparation::Candidate(_)) => {}
                    Self::Resident(owned) => {
                        if owned.identity != identity {
                            return Err(HostError::internal(
                                "resident Session Environment projection changed its durable identity",
                            ));
                        }
                    }
                    Self::Retiring(retiring) => match &retiring.owned {
                        RetiringEnvironmentOwner::Bound(owned) => {
                            if owned.identity != identity {
                                return Err(HostError::internal(
                                    "retiring Session Environment projection changed its durable identity",
                                ));
                            }
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
        kind: awaken_session_contract::SessionEnvironmentEffectKind,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        let binding = serde_json::to_string(&environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let origin = match (&mut *self, kind) {
            (Self::Vacant, awaken_session_contract::SessionEnvironmentEffectKind::Create) => {
                UnboundSessionEnvironmentOrigin::New
            }
            (Self::Vacant, awaken_session_contract::SessionEnvironmentEffectKind::Adopt) => {
                UnboundSessionEnvironmentOrigin::Adoption
            }
            (
                Self::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                    identity,
                    binding: expected,
                }),
                awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
            ) if *expected == binding => {
                UnboundSessionEnvironmentOrigin::DurableAdoption(identity.clone())
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
        request: &awaken_session_contract::SandboxRestoreRequest,
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

    pub(super) fn complete_restore_target_disposal(
        &mut self,
        request: &awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<(), HostError> {
        let Self::Restoring(SessionEnvironmentRestoration::Awaiting { request: current }) = self
        else {
            return Err(HostError::internal(
                "restored-target disposal has no exact Restoring owner fence",
            ));
        };
        if current != request {
            return Err(HostError::internal(
                "restored-target disposal does not match its durable request",
            ));
        }
        *self = Self::Vacant;
        Ok(())
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

    pub(super) fn reactivate_retiring(
        &mut self,
        expected: &RetiringSessionEnvironment,
    ) -> Result<(), HostError> {
        let Self::Retiring(current) = self else {
            return Err(HostError::internal(
                "Session Environment reactivation has no Retiring owner",
            ));
        };
        if !current.exact_matches(expected) {
            return Err(HostError::internal(
                "Session Environment reactivation lost its exact owner fence",
            ));
        }
        if !matches!(
            &current.cause,
            SessionEnvironmentRetirementCause::RecoveryDiscard
                | SessionEnvironmentRetirementCause::RealizationRevocation
        ) {
            return Err(HostError::internal(
                "this Session Environment retirement cause cannot be reactivated",
            ));
        }
        let RetiringEnvironmentOwner::Bound(owned) = &current.owned else {
            return Err(HostError::internal(
                "an unpersisted Environment candidate cannot be reactivated",
            ));
        };
        *self = Self::Resident(owned.clone());
        Ok(())
    }
}

pub(super) fn committed_identity(
    receipt: &awaken_session_contract::SessionEnvironmentReceipt,
    committed: awaken_session_contract::SessionEnvironmentState,
) -> Result<BoundSessionEnvironmentIdentity, HostError> {
    let awaken_session_contract::SessionEnvironmentState::Resident {
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
