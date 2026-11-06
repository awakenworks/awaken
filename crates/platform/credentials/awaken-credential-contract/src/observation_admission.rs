// @formal entrypoint worker_credential.exact_revision_and_current_epoch_required
// @formal entrypoint worker_credential.expired_observation_never_executes
//! Canonical admission of Worker-local credential observations.
//!
//! Provider adapters report liveness, but the neutral credential contract owns
//! the exact identity/state decision. Callers must not treat an arbitrary `Ok`
//! payload as proof that the placement-pinned reference is still usable.

use crate::{
    CredentialMaterialError, CredentialObservation, CredentialObservationState, CredentialRef,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerReferenceRevalidationDisposition {
    Available,
    RevisionMismatch,
    LoginRequired,
    Expired,
    Invalid,
    Disabled,
    ProbeFailed,
}

#[must_use]
const fn worker_reference_revalidation_disposition(
    reference_matches: bool,
    state: CredentialObservationState,
) -> WorkerReferenceRevalidationDisposition {
    if !reference_matches {
        return WorkerReferenceRevalidationDisposition::RevisionMismatch;
    }
    match state {
        CredentialObservationState::Available => WorkerReferenceRevalidationDisposition::Available,
        CredentialObservationState::LoginRequired => {
            WorkerReferenceRevalidationDisposition::LoginRequired
        }
        CredentialObservationState::Expired => WorkerReferenceRevalidationDisposition::Expired,
        CredentialObservationState::Invalid => WorkerReferenceRevalidationDisposition::Invalid,
        CredentialObservationState::Disabled => WorkerReferenceRevalidationDisposition::Disabled,
        CredentialObservationState::ProbeFailed => {
            WorkerReferenceRevalidationDisposition::ProbeFailed
        }
    }
}

/// Require one use-time observation to confirm the exact placement-pinned
/// reference in the only state that may enter execution.
///
/// This is intentionally stricter than accepting a resolver's `Ok`: an adapter
/// cannot substitute another revision or encode a negative liveness state as a
/// successful transport response.
pub fn require_exact_available_worker_reference(
    expected: &CredentialRef,
    observation: CredentialObservation,
) -> Result<CredentialObservation, CredentialMaterialError> {
    let disposition = worker_reference_revalidation_disposition(
        observation.credential == *expected,
        observation.state,
    );
    match disposition {
        WorkerReferenceRevalidationDisposition::Available => Ok(observation),
        WorkerReferenceRevalidationDisposition::RevisionMismatch => {
            Err(CredentialMaterialError::RevisionMismatch)
        }
        WorkerReferenceRevalidationDisposition::LoginRequired => {
            Err(CredentialMaterialError::LoginRequired)
        }
        WorkerReferenceRevalidationDisposition::Expired => Err(CredentialMaterialError::Expired),
        WorkerReferenceRevalidationDisposition::Invalid => Err(CredentialMaterialError::Invalid),
        WorkerReferenceRevalidationDisposition::Disabled => Err(CredentialMaterialError::Disabled),
        WorkerReferenceRevalidationDisposition::ProbeFailed => {
            Err(CredentialMaterialError::ProbeFailed)
        }
    }
}

#[cfg(any(test, kani))]
fn proof_observation(
    credential: CredentialRef,
    state: CredentialObservationState,
) -> CredentialObservation {
    CredentialObservation {
        credential,
        state,
        observed_at_ms: 10,
        reason_code: None,
    }
}

#[cfg(kani)]
/// @obligation worker_credential.exact_revision_and_current_epoch_required
#[kani::proof]
fn worker_reference_revision_comparison_is_full_width() {
    // Causes: C1 expected and observed revisions are independent full-width
    // u64 values; C2 id is exact; C3 state Available. Effects: R1 C1 equal
    // succeeds; R2 any changed revision rejects before effects.
    let expected_revision: u64 = kani::any();
    let observed_revision: u64 = kani::any();
    let expected = CredentialRef {
        id: "a".into(),
        revision: expected_revision,
    };
    let observation = proof_observation(
        CredentialRef {
            id: expected.id.clone(),
            revision: observed_revision,
        },
        CredentialObservationState::Available,
    );
    let result = require_exact_available_worker_reference(&expected, observation);
    if expected_revision == observed_revision {
        assert!(result.is_ok());
    } else {
        assert!(matches!(
            result,
            Err(CredentialMaterialError::RevisionMismatch)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_time_revalidation_requires_exact_reference_and_available_state() {
        // Cause/effect decision table:
        // R1 exact id+revision + Available -> return the observation.
        // R2 different id or revision + any state -> RevisionMismatch.
        // R3 exact reference + each negative state -> its typed liveness error.
        // Effects: only R1 can proceed to the claim-current execution fence;
        // every other rule stops before model, tool, or Sandbox entry.
        let expected = CredentialRef {
            id: "cred:worker".into(),
            revision: 7,
        };
        let observation = |credential, state| CredentialObservation {
            credential,
            state,
            observed_at_ms: 10,
            reason_code: None,
        };

        assert!(
            require_exact_available_worker_reference(
                &expected,
                observation(expected.clone(), CredentialObservationState::Available),
            )
            .is_ok(),
            "R1"
        );
        for other in [
            CredentialRef {
                id: "cred:other".into(),
                revision: 7,
            },
            CredentialRef {
                id: expected.id.clone(),
                revision: 6,
            },
        ] {
            for state in [
                CredentialObservationState::Available,
                CredentialObservationState::LoginRequired,
                CredentialObservationState::Expired,
                CredentialObservationState::Invalid,
                CredentialObservationState::Disabled,
                CredentialObservationState::ProbeFailed,
            ] {
                assert_eq!(
                    require_exact_available_worker_reference(
                        &expected,
                        observation(other.clone(), state),
                    ),
                    Err(CredentialMaterialError::RevisionMismatch),
                    "R2"
                );
            }
        }
        for (state, error) in [
            (
                CredentialObservationState::LoginRequired,
                CredentialMaterialError::LoginRequired,
            ),
            (
                CredentialObservationState::Expired,
                CredentialMaterialError::Expired,
            ),
            (
                CredentialObservationState::Invalid,
                CredentialMaterialError::Invalid,
            ),
            (
                CredentialObservationState::Disabled,
                CredentialMaterialError::Disabled,
            ),
            (
                CredentialObservationState::ProbeFailed,
                CredentialMaterialError::ProbeFailed,
            ),
        ] {
            assert_eq!(
                require_exact_available_worker_reference(
                    &expected,
                    observation(expected.clone(), state),
                ),
                Err(error),
                "R3"
            );
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const fn listed_state(state: CredentialObservationState) -> usize {
        match state {
            CredentialObservationState::Available => 0,
            CredentialObservationState::LoginRequired => 1,
            CredentialObservationState::Expired => 2,
            CredentialObservationState::Invalid => 3,
            CredentialObservationState::Disabled => 4,
            CredentialObservationState::ProbeFailed => 5,
        }
    }
    const EVERY_STATE: [CredentialObservationState; 6] = [
        CredentialObservationState::Available,
        CredentialObservationState::LoginRequired,
        CredentialObservationState::Expired,
        CredentialObservationState::Invalid,
        CredentialObservationState::Disabled,
        CredentialObservationState::ProbeFailed,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_STATE.into_iter().enumerate() {
            assert_eq!(listed_state(value), index);
        }
    }

    /// @exhaustive revision{0,u64::MAX}(2) x bool(2) = 4
    /// @obligation worker_credential.exact_revision_and_current_epoch_required
    #[test]
    fn worker_reference_id_is_derived_from_real_refs() {
        // Causes: C1 id exact/foreign; C2 revision is the same boundary value, 0
        // or MAX; C3 state Available. Effects: R1 C1 exact succeeds; R2 foreign
        // id returns RevisionMismatch. The production door still computes
        // CredentialRef equality from the constructed references, not from the
        // selector.
        for revision in [0, u64::MAX] {
            for same_id in [false, true] {
                let expected = CredentialRef {
                    id: "a".into(),
                    revision,
                };
                let observed = CredentialRef {
                    id: if same_id {
                        expected.id.clone()
                    } else {
                        "b".into()
                    },
                    revision,
                };
                let result = require_exact_available_worker_reference(
                    &expected,
                    proof_observation(observed, CredentialObservationState::Available),
                );
                if same_id {
                    assert!(result.is_ok());
                } else {
                    assert!(matches!(
                        result,
                        Err(CredentialMaterialError::RevisionMismatch)
                    ));
                }
            }
        }
    }

    /// @exhaustive CredentialObservationState(6) = 6
    /// @obligation worker_credential.expired_observation_never_executes
    #[test]
    fn exact_worker_reference_maps_every_observation_state() {
        // Causes: C1 one exact id+revision; C2 each of six states. Effects: R1
        // Available succeeds; R2 every negative state returns its
        // own typed error, with no successful materialization decision.
        let expected = CredentialRef {
            id: "a".into(),
            revision: 7,
        };
        for state in EVERY_STATE {
            let observation = proof_observation(expected.clone(), state);
            let result = require_exact_available_worker_reference(&expected, observation);
            match state {
                CredentialObservationState::Available => assert!(result.is_ok()),
                CredentialObservationState::LoginRequired => {
                    assert!(matches!(
                        result,
                        Err(CredentialMaterialError::LoginRequired)
                    ));
                }
                CredentialObservationState::Expired => {
                    assert!(matches!(result, Err(CredentialMaterialError::Expired)));
                }
                CredentialObservationState::Invalid => {
                    assert!(matches!(result, Err(CredentialMaterialError::Invalid)));
                }
                CredentialObservationState::Disabled => {
                    assert!(matches!(result, Err(CredentialMaterialError::Disabled)));
                }
                CredentialObservationState::ProbeFailed => {
                    assert!(matches!(result, Err(CredentialMaterialError::ProbeFailed)));
                }
            }
        }
    }
}
