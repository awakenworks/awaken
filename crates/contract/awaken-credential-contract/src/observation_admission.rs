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
/// @obligation worker_credential.exact_revision_and_current_epoch_required
/// @obligation worker_credential.expired_observation_never_executes
#[kani::proof]
fn worker_reference_revalidation_accepts_only_exact_available_state() {
    let reference_matches = kani::any::<bool>();
    let tag = kani::any::<u8>();
    kani::assume(tag < 6);
    let state = match tag {
        0 => CredentialObservationState::Available,
        1 => CredentialObservationState::LoginRequired,
        2 => CredentialObservationState::Expired,
        3 => CredentialObservationState::Invalid,
        4 => CredentialObservationState::Disabled,
        _ => CredentialObservationState::ProbeFailed,
    };
    let admitted = worker_reference_revalidation_disposition(reference_matches, state)
        == WorkerReferenceRevalidationDisposition::Available;

    assert_eq!(
        admitted,
        reference_matches && state == CredentialObservationState::Available
    );
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
            assert_eq!(
                require_exact_available_worker_reference(
                    &expected,
                    observation(other, CredentialObservationState::Available),
                ),
                Err(CredentialMaterialError::RevisionMismatch),
                "R2"
            );
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
