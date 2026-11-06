// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-application/src/creation.rs
// @formal reason: cfg(test) only; verifies failure classes and permanent versus transient effects at the application boundary.
use super::*;
use awaken_runtime_contract::runs::RunErrorKind;
use awaken_session_contract::SessionRepositoryError as R;
use awaken_session_execution_contract::{
    SessionRealizationControlFailure as C, SessionRealizationDriveError as D,
};

#[test]
fn durable_failures_keep_their_class_across_creation_realization() {
    // C1/C2 damaged root/invalid CAS are permanent; C3 authority outage is
    // retryable. Identical prose must not affect either repository or Control.
    for (cause, permanent) in [
        (R::Corrupt("same detail".into()), true),
        (R::InvalidMutation("same detail".into()), true),
        (R::AuthorityUnavailable("same detail".into()), false),
    ] {
        let direct = SessionCreationError::repository(cause.clone());
        let control = crate::realization::repository_control(cause);
        if permanent {
            assert_eq!(control, C::Internal("same detail".into()));
        } else {
            assert_eq!(control, C::Unavailable("same detail".into()));
        }
        for error in [
            direct,
            SessionCreationError::realization(D::Control(control)),
        ] {
            let run: RunError = match error {
                SessionCreationError::Internal(detail) => {
                    SessionMutationError::Internal(detail).into()
                }
                SessionCreationError::Unavailable(detail) => RunError::unavailable(detail),
                SessionCreationError::Rejected(error) => error,
                error => panic!("{error:?}"),
            };
            assert_eq!(
                run.kind,
                if permanent {
                    RunErrorKind::Internal
                } else {
                    RunErrorKind::Unavailable
                }
            );
            assert!(run.message.contains("same detail"));
            assert!(!run.is_unclassified_internal());
        }
    }
}

#[test]
fn creation_preserves_effect_kind_code_and_exact_message() {
    for error in [
        RunError::classified("known_permanent", "same detail"),
        RunError::unavailable("same detail"),
    ] {
        let expected = (error.kind, error.code.clone(), error.message.clone());
        let SessionCreationError::Rejected(projected) =
            SessionCreationError::realization(D::Effect(error.into()))
        else {
            panic!("effect was flattened")
        };
        assert_eq!(
            (projected.kind, projected.code, projected.message),
            expected
        );
    }
}

#[test]
fn only_outages_and_invalid_state_leave_create_adoption_unknown() {
    // A1 outage, A2 fixed invariant, A3 typed aggregate refusal: the replay
    // cannot prove the receipt absent, so participants are preserved.
    // A4-A7 deterministic refusals prove this exact receipt was not adopted.
    for (error, unknown) in [
        (SessionCreationError::Unavailable("outage".into()), true),
        (SessionCreationError::Internal("invariant".into()), true),
        (
            SessionCreationError::Invalid(crate::SessionStateViolation::Aggregate(
                awaken_session_contract::SessionMutationValidationError::ReplacementRevisionMismatch,
            )),
            true,
        ),
        (SessionCreationError::Conflict, false),
        (SessionCreationError::Tombstoned, false),
        (SessionCreationError::IdempotencyMismatch, false),
        (
            SessionCreationError::Rejected(RunError::bad_request("refused")),
            false,
        ),
    ] {
        assert_eq!(error.leaves_adoption_unknown(), unknown, "{error:?}");
    }
}
