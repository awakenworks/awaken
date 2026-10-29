// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-application/src/run_admission.rs
// @formal reason: cfg(test) only; verifies failure classes and permanent versus transient effects at the application boundary.
use super::*;
use awaken_session_types::RunErrorKind;

#[test]
fn admission_keeps_durable_failures_permanent_through_each_adapter() {
    // Paired oracle: identical detail, different cause, from repository through
    // Control/creation/admission. Known permanent failures cannot use the
    // unclassified internal code that permits an effect retry budget.
    for (cause, kind) in [
        (
            SessionRepositoryError::Corrupt("same detail".into()),
            RunErrorKind::Internal,
        ),
        (
            SessionRepositoryError::InvalidMutation("same detail".into()),
            RunErrorKind::Internal,
        ),
        (
            SessionRepositoryError::AuthorityUnavailable("same detail".into()),
            RunErrorKind::Unavailable,
        ),
    ] {
        let control = crate::realization::repository_control(cause.clone());
        let errors = [
            awaken_session_realization_contract::SessionRealizationDriveError::Control(control)
                .into(),
            creation_error(SessionCreationError::repository(cause.clone())),
            mutation_error(cause.clone().into()),
            session_repository_error(cause),
        ];
        for error in errors {
            assert_eq!(error.kind, kind);
            assert!(!error.is_unclassified_internal());
            assert!(error.message.contains("same detail"));
        }
    }
}
