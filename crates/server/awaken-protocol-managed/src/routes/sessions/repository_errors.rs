// @formal surface exclude=test_only
// @formal owner crates/server/awaken-protocol-managed/src/routes/sessions.rs
// @formal reason: Compiled only by cfg(test); tests typed failure classes through the HTTP response adapter.
//! The application and direct-read paths must expose the same HTTP failures.
use super::error_response;
use crate::state::StateError;
use awaken_session_application::SessionMutationError;
use awaken_session_contract::{SessionRepositoryConflict, SessionRepositoryError};
use axum::http::StatusCode;

#[test]
fn repository_errors_keep_identical_http_classes_through_application_and_direct_reads() {
    // Cause/effect decision table: R1 missing -> 404; R2-R4 conflicting root or
    // identity -> 409; R5 outage -> 503; R6-R7 corruption/invalid command -> 500.
    // Deliberately identical, misleading details prevent message-based routing.
    // Both independently reached adapter paths must satisfy the wire oracle.
    let detail = "offline corrupt conflict not found";
    for (cause, expected, kind) in [
        (
            SessionRepositoryError::NotFound,
            StatusCode::NOT_FOUND,
            "not_found_error",
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::AlreadyExists),
            StatusCode::CONFLICT,
            "invalid_request_error",
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::Tombstoned),
            StatusCode::CONFLICT,
            "invalid_request_error",
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::IdempotencyMismatch),
            StatusCode::CONFLICT,
            "invalid_request_error",
        ),
        (
            SessionRepositoryError::AuthorityUnavailable(detail.into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
        ),
        (
            SessionRepositoryError::Corrupt(detail.into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
        ),
        (
            SessionRepositoryError::InvalidMutation(detail.into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
        ),
    ] {
        let application = StateError::from(SessionMutationError::from(cause.clone()));
        for (path, error) in [
            ("application", application),
            ("direct", StateError::from(cause.clone())),
        ] {
            if let StateError::Run(run) = &error
                && expected == StatusCode::INTERNAL_SERVER_ERROR
            {
                assert!(
                    !run.is_unclassified_internal(),
                    "{path}: known durable failure"
                );
            }
            let (status, body) = error_response(error);
            assert_eq!(status, expected, "{path}: {cause:?}");
            assert_eq!(body.0.error.kind, kind, "{path}: {cause:?}");
            if expected.is_server_error() {
                assert!(
                    body.0.error.message.contains(detail),
                    "{path}: cause detail retained"
                );
            }
        }
    }
    let (status, _) = error_response(StateError::from(SessionMutationError::Internal(
        "invalid local receipt".into(),
    )));
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
