// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! The pure decisions of the Environment owner: which owner a durable
//! Environment state projects, whether a durable projection is the exact owner
//! or a stale stutter of it, and which commit a Candidate receipt admits. Each
//! is a total function over its causes; the owner transitions in `state` apply
//! the decision, and the decision tables below cover every row.

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance, LegacyEnvironmentEffect,
    UnboundSessionEnvironmentOrigin,
};
use crate::realization_error::RealizationError;

pub enum ProjectedEnvironmentOwner {
    Vacant,
    AwaitingAdoption {
        identity: BoundSessionEnvironmentIdentity,
        binding: String,
    },
    Restoring {
        request: awaken_provisioning_contract::SandboxRestoreRequest,
    },
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
) -> Result<ProjectedEnvironmentOwner, RealizationError> {
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
                    return Err(RealizationError::invariant(
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

pub(super) fn same_durable_generation(
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
pub(super) enum DurableProjectionIdentityDecision {
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
pub(super) enum CandidateCommitDecision {
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
pub(super) fn candidate_receipt_decision(
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

pub(super) const fn durable_projection_identity_decision(
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_types::SessionId;
    use {
        awaken_provisioning_contract::SandboxCheckpointRef,
        awaken_session_contract::SandboxGeneration,
        awaken_session_contract::SessionEnvironmentOperation,
        awaken_session_contract::SessionEnvironmentState, awaken_session_contract::SuspendPhase,
    };

    fn generation(id: u64) -> SandboxGeneration {
        SandboxGeneration::new(&SessionId::from("session"), id, u64::MAX, "env", "image")
    }

    fn durable(effect: &str, generation_id: u64) -> BoundSessionEnvironmentIdentity {
        BoundSessionEnvironmentIdentity::Durable {
            effect_id: effect.into(),
            generation: generation(generation_id),
        }
    }

    fn legacy(binding: &str, effect: LegacyEnvironmentEffect) -> BoundSessionEnvironmentIdentity {
        BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::DurableBinding {
                binding: binding.into(),
                effect,
            },
        )
    }

    fn checkpoint(generation: &SandboxGeneration) -> SandboxCheckpointRef {
        let suspend = SessionEnvironmentOperation::new(
            "workspace",
            &SessionId::from("session"),
            "suspend",
            generation,
            1,
            None,
            None,
        );
        SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "portable-test-v1".into(),
            digest: "digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: generation.environment_fingerprint().to_owned(),
            base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: suspend.effect_id().to_owned(),
        }
    }

    /// | identity exact | binding exact | generation exact | decision |
    /// | yes | yes | any | Exact |
    /// | no  | yes | yes | SamePhysicalOwnerStutter |
    /// | no  | yes | no  | Conflict |
    /// | any | no  | any | Conflict |
    #[test]
    fn durable_projection_identity_decision_covers_every_row() {
        use DurableProjectionIdentityDecision::{Conflict, Exact, SamePhysicalOwnerStutter};
        for (identity, binding, generation, expected) in [
            (true, true, true, Exact),
            (true, true, false, Exact),
            (false, true, true, SamePhysicalOwnerStutter),
            (false, true, false, Conflict),
            (true, false, true, Conflict),
            (true, false, false, Conflict),
            (false, false, true, Conflict),
            (false, false, false, Conflict),
        ] {
            assert_eq!(
                durable_projection_identity_decision(identity, binding, generation),
                expected,
                "identity={identity} binding={binding} generation={generation}"
            );
        }
    }

    /// | Candidate class | committed exact | source compatible | decision |
    /// | New | any | any | BindCreate |
    /// | Adoption | any | any | BindAdoption |
    /// | AdoptionFrom | any | yes | BindAdoption |
    /// | AdoptionFrom | any | no | Conflict |
    /// | DurableCreate / DurableAdoption | yes | any | Replay |
    /// | DurableCreate / DurableAdoption | no | any | Conflict |
    #[test]
    fn candidate_commit_decision_covers_every_class_and_witness() {
        use CandidateCommitClass as Class;
        use CandidateCommitDecision as Decision;
        for class in [
            Class::New,
            Class::Adoption,
            Class::AdoptionFrom,
            Class::DurableCreate,
            Class::DurableAdoption,
        ] {
            for exact in [false, true] {
                for compatible in [false, true] {
                    let expected = match class {
                        Class::New => Decision::BindCreate,
                        Class::Adoption => Decision::BindAdoption,
                        Class::AdoptionFrom if compatible => Decision::BindAdoption,
                        Class::AdoptionFrom => Decision::Conflict,
                        Class::DurableCreate | Class::DurableAdoption if exact => Decision::Replay,
                        Class::DurableCreate | Class::DurableAdoption => Decision::Conflict,
                    };
                    assert_eq!(
                        candidate_commit_decision(class, exact, compatible),
                        expected,
                        "{class:?} exact={exact} compatible={compatible}"
                    );
                }
            }
        }
    }

    /// | origin | receipt identity | decision |
    /// | New | any | BindCreate |
    /// | Adoption | any | BindAdoption |
    /// | AdoptionFrom(S) | S | BindAdoption |
    /// | AdoptionFrom(Durable g) | Durable g, another effect | BindAdoption |
    /// | AdoptionFrom(legacy binding) | Durable | BindAdoption (typed upgrade) |
    /// | AdoptionFrom(Durable g) | Durable g' (g' != g) | Conflict |
    /// | AdoptionFrom(Durable g) | legacy binding | Conflict |
    /// | DurableCreate(C) / DurableAdoption(C) | C | Replay |
    /// | DurableCreate(C) / DurableAdoption(C) | not C | Conflict |
    #[test]
    fn candidate_receipt_decision_admits_only_compatible_receipts() {
        use CandidateCommitDecision as Decision;
        use UnboundSessionEnvironmentOrigin as Origin;
        let source = durable("source", 1);
        let legacy_source = legacy("binding", LegacyEnvironmentEffect::Absent);
        for (origin, identity, expected, row) in [
            (Origin::New, durable("next", 2), Decision::BindCreate, "new"),
            (
                Origin::Adoption,
                durable("next", 2),
                Decision::BindAdoption,
                "adoption",
            ),
            (
                Origin::AdoptionFrom(source.clone()),
                source.clone(),
                Decision::BindAdoption,
                "same source",
            ),
            (
                Origin::AdoptionFrom(source.clone()),
                durable("renewed", 1),
                Decision::BindAdoption,
                "same physical generation",
            ),
            (
                Origin::AdoptionFrom(legacy_source.clone()),
                durable("upgraded", 3),
                Decision::BindAdoption,
                "typed legacy upgrade",
            ),
            (
                Origin::AdoptionFrom(source.clone()),
                durable("other", 2),
                Decision::Conflict,
                "another generation",
            ),
            (
                Origin::AdoptionFrom(source.clone()),
                legacy_source.clone(),
                Decision::Conflict,
                "durable relabelled as legacy",
            ),
            (
                Origin::DurableCreate(source.clone()),
                source.clone(),
                Decision::Replay,
                "create replay",
            ),
            (
                Origin::DurableAdoption(source.clone()),
                source.clone(),
                Decision::Replay,
                "adoption replay",
            ),
            (
                Origin::DurableCreate(source.clone()),
                durable("renewed", 1),
                Decision::Conflict,
                "create relabel",
            ),
            (
                Origin::DurableAdoption(source.clone()),
                durable("other", 2),
                Decision::Conflict,
                "adoption relabel",
            ),
        ] {
            assert_eq!(
                candidate_receipt_decision(&origin, &identity),
                expected,
                "{row}"
            );
        }
    }

    /// | aggregate state | projected owner |
    /// | Unmaterialized / Hibernated | Vacant |
    /// | Resident with effect and generation | AwaitingAdoption(Durable) |
    /// | Resident without generation | AwaitingAdoption(legacy, Known or Absent effect) |
    /// | Resident with generation but no effect | rejected (fail closed) |
    /// | Suspending with a source effect | AwaitingAdoption(Durable source) |
    /// | Suspending without a source effect | AwaitingAdoption(legacy source binding) |
    /// | Restoring | Restoring(the exact restore request) |
    #[test]
    fn projected_environment_owner_covers_every_state_and_rejects_untyped_generations() {
        let resident = |effect_id: Option<&str>, generation: Option<SandboxGeneration>| {
            SessionEnvironmentState::Resident {
                binding: "binding".into(),
                effect_id: effect_id.map(str::to_owned),
                generation,
                idle_since_unix_ms: None,
            }
        };
        let awaiting = |state: &SessionEnvironmentState| match projected_environment_owner(
            state,
            "workspace",
            "session",
        ) {
            Ok(ProjectedEnvironmentOwner::AwaitingAdoption { identity, binding }) => {
                (identity, binding)
            }
            Ok(_) => panic!("expected an awaiting adoption"),
            Err(error) => panic!("unexpected rejection: {error}"),
        };
        let vacant = |state: &SessionEnvironmentState| {
            matches!(
                projected_environment_owner(state, "workspace", "session"),
                Ok(ProjectedEnvironmentOwner::Vacant)
            )
        };
        assert!(
            vacant(&SessionEnvironmentState::Unmaterialized),
            "unmaterialized"
        );
        let hibernated_generation = generation(4);
        assert!(
            vacant(&SessionEnvironmentState::Hibernated {
                checkpoint: checkpoint(&hibernated_generation),
                generation: hibernated_generation,
            }),
            "hibernated"
        );
        assert_eq!(
            awaiting(&resident(Some("effect"), Some(generation(1)))),
            (durable("effect", 1), "binding".into()),
            "durable resident"
        );
        assert_eq!(
            awaiting(&resident(Some("effect"), None)),
            (
                legacy("binding", LegacyEnvironmentEffect::Known("effect".into())),
                "binding".into()
            ),
            "legacy resident with a known effect"
        );
        assert_eq!(
            awaiting(&resident(None, None)),
            (
                legacy("binding", LegacyEnvironmentEffect::Absent),
                "binding".into()
            ),
            "legacy resident without an effect"
        );
        assert!(
            projected_environment_owner(
                &resident(None, Some(generation(1))),
                "workspace",
                "session"
            )
            .is_err(),
            "a generation without its effect identity fails closed"
        );
        let suspending = |source_effect_id: &str| {
            let generation = generation(2);
            SessionEnvironmentState::Suspending {
                operation: SessionEnvironmentOperation::new(
                    "workspace",
                    &SessionId::from("session"),
                    "suspend",
                    &generation,
                    2,
                    None,
                    None,
                ),
                source_effect_id: Box::new(source_effect_id.into()),
                source_binding: "source-binding".into(),
                generation,
                suspend_phase: SuspendPhase::Quiescing,
                checkpoint: None,
                source_release_preparation: None,
            }
        };
        assert_eq!(
            awaiting(&suspending("source")),
            (durable("source", 2), "source-binding".into()),
            "durable suspending source"
        );
        assert_eq!(
            awaiting(&suspending("")),
            (
                legacy("source-binding", LegacyEnvironmentEffect::Absent),
                "source-binding".into()
            ),
            "legacy suspending source"
        );
        let restoring_generation = generation(3);
        let restore_checkpoint = checkpoint(&restoring_generation);
        let restoring = SessionEnvironmentState::Restoring {
            operation: SessionEnvironmentOperation::new(
                "workspace",
                &SessionId::from("session"),
                "restore",
                &restoring_generation,
                3,
                None,
                Some(&restore_checkpoint),
            ),
            checkpoint: restore_checkpoint,
            generation: restoring_generation,
        };
        match projected_environment_owner(&restoring, "workspace", "session") {
            Ok(ProjectedEnvironmentOwner::Restoring { request }) => assert_eq!(
                Some(request),
                restoring.restoring_request("workspace", &SessionId::from("session")),
                "restoring projects its exact request"
            ),
            _ => panic!("restoring projects a restore fence"),
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_CLASS: [CandidateCommitClass; 5] = [
        CandidateCommitClass::New,
        CandidateCommitClass::Adoption,
        CandidateCommitClass::DurableCreate,
        CandidateCommitClass::DurableAdoption,
        CandidateCommitClass::AdoptionFrom,
    ];
    const BOOLS: [bool; 2] = [false, true];

    const fn listed_class(class: CandidateCommitClass) -> usize {
        match class {
            CandidateCommitClass::New => 0,
            CandidateCommitClass::Adoption => 1,
            CandidateCommitClass::DurableCreate => 2,
            CandidateCommitClass::DurableAdoption => 3,
            CandidateCommitClass::AdoptionFrom => 4,
        }
    }

    #[test]
    fn every_candidate_commit_class_is_listed() {
        for (index, class) in EVERY_CLASS.into_iter().enumerate() {
            assert_eq!(listed_class(class), index);
        }
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) = 8
    /// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
    #[test]
    fn durable_projection_identity_is_exact_or_a_same_physical_owner_stutter() {
        // C1 effect identity is exact; C2 binding is exact; C3 immutable
        // generation is exact. E1 C1+C2 => Exact. E2 !C1+C2+C3 => stale
        // same-owner stutter. E3 every remaining combination => Conflict.
        let mut cases = 0;
        for identity_is_exact in BOOLS {
            for binding_is_exact in BOOLS {
                for generation_is_exact in BOOLS {
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
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 8);
    }

    /// @exhaustive CandidateCommitClass(5) x bool(2) x bool(2) = 20
    /// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
    #[test]
    fn committed_candidate_retains_effect_kind_and_rejects_identity_relabeling() {
        // C1 is the closed Candidate origin class; C2 a previously committed
        // identity is exact. E1 New binds DurableCreate; E2 Adoption binds
        // DurableAdoption; source adoption binds only a compatible physical
        // generation. E3 either committed class plus exact C2 replays; E4
        // either durable class plus !C2 conflicts.
        let mut cases = 0;
        for class in EVERY_CLASS {
            for exact in BOOLS {
                for compatible in BOOLS {
                    let decision = candidate_commit_decision(class, exact, compatible);
                    match class {
                        CandidateCommitClass::New => {
                            assert_eq!(decision, CandidateCommitDecision::BindCreate)
                        }
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
                        CandidateCommitClass::DurableCreate
                        | CandidateCommitClass::DurableAdoption
                            if exact =>
                        {
                            assert_eq!(decision, CandidateCommitDecision::Replay)
                        }
                        CandidateCommitClass::DurableCreate
                        | CandidateCommitClass::DurableAdoption => {
                            assert_eq!(decision, CandidateCommitDecision::Conflict)
                        }
                    }
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 20);
    }
}
