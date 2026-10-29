// @formal surface exclude=test_only
// @formal owner crates/server/awaken-protocol-managed/src/state/session_update.rs
// @formal reason: cfg(test) only; verifies failure classes and permanent versus transient effects at the application boundary.
use super::*;
use awaken_session_application::SessionUpdateError as U;
use awaken_session_realization_contract::{
    SessionRealizationControlFailure as C, SessionRealizationDriveError as D,
};
use axum::http::StatusCode;

#[test]
fn update_and_realization_http_classes_are_cause_based() {
    // U1 absent =>404, U2 CAS/identity =>409, U3 caller invalid =>400,
    // U4 temporary authority outage =>503, U5 durable state invalid =>500.
    // Test the actual update adapter followed by the public HTTP projection.
    for (error, expected) in [
        (U::NotFound, StatusCode::NOT_FOUND),
        (
            U::Realization(D::DidNotConverge),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (U::Conflict, StatusCode::CONFLICT),
        (U::IdempotencyMismatch, StatusCode::CONFLICT),
        (
            U::Unavailable("same detail".into()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            U::Rejected(
                awaken_session_application::SessionMutationError::Internal("same detail".into())
                    .into(),
            ),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            U::Realization(D::Control(C::NotFound)),
            StatusCode::NOT_FOUND,
        ),
        (
            U::Realization(D::Control(C::NotReady)),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (U::Realization(D::Control(C::Retired)), StatusCode::CONFLICT),
        (
            U::Realization(D::Control(C::Terminal)),
            StatusCode::CONFLICT,
        ),
        (
            U::Realization(D::Control(C::StaleOwnership)),
            StatusCode::CONFLICT,
        ),
        (
            U::Realization(D::Control(C::Conflict)),
            StatusCode::CONFLICT,
        ),
        (
            U::Realization(D::Control(C::Invalid("same detail".into()))),
            StatusCode::BAD_REQUEST,
        ),
        (
            U::Realization(D::Control(C::Unavailable("same detail".into()))),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            U::Realization(D::Control(C::Internal("same detail".into()))),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let projected = ManagedState::map_update_error(error);
        if let StateError::Run(run) = &projected
            && expected == StatusCode::INTERNAL_SERVER_ERROR
        {
            assert!(
                !run.is_unclassified_internal(),
                "known corruption cannot enter effect retry budget"
            );
        }
        let (status, _) = crate::routes::sessions::error_response(projected);
        assert_eq!(status, expected);
    }
}
