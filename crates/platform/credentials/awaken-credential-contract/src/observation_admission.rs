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

#[cfg(kani)]
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
fn proof_observation_state() -> CredentialObservationState {
    let tag: u8 = kani::any();
    kani::assume(tag < 6);
    match tag {
        0 => CredentialObservationState::Available,
        1 => CredentialObservationState::LoginRequired,
        2 => CredentialObservationState::Expired,
        3 => CredentialObservationState::Invalid,
        4 => CredentialObservationState::Disabled,
        _ => CredentialObservationState::ProbeFailed,
    }
}

#[cfg(kani)]
/// @obligation worker_credential.exact_revision_and_current_epoch_required
#[kani::proof]
fn worker_reference_id_is_derived_from_real_refs() {
    // Causes: C1 id exact/foreign; C2 revision is the same boundary value, 0
    // or MAX; C3 state Available. Effects: R1 C1 exact succeeds; R2 foreign
    // id returns RevisionMismatch. The one-byte IDs bound String comparison
    // cost; the production door still computes CredentialRef equality from
    // the constructed references, not from the symbolic selector.
    let revision = if kani::any::<bool>() { 0 } else { u64::MAX };
    let expected = CredentialRef {
        id: "a".into(),
        revision,
    };
    let same_id: bool = kani::any();
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

#[cfg(kani)]
/// @obligation worker_credential.expired_observation_never_executes
#[kani::proof]
fn exact_worker_reference_maps_every_observation_state() {
    // Causes: C1 one exact id+revision; C2 each of six states. Effects: R1
    // Available succeeds; R2 every negative state returns its
    // own typed error, with no successful materialization decision.
    let expected = CredentialRef {
        id: "a".into(),
        revision: 7,
    };
    let state = proof_observation_state();
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
