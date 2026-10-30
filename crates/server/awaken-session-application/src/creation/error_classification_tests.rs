// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-application/src/creation.rs
// @formal reason: cfg(test) only; verifies failure classes and permanent versus transient effects at the application boundary.
use super::*;
use awaken_session_contract::SessionRepositoryError as R;
use awaken_session_realization_contract::{
    SessionRealizationControlFailure as C, SessionRealizationDriveError as D,
};
use awaken_session_types::RunErrorKind;

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
