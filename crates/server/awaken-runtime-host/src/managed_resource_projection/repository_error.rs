//! Repository binding failures mapped at the Managed resource boundary.
use awaken_resource_contract::RepositoryBindingVerifierError;
use awaken_session_error_contract::RunError;

pub(super) fn repository_binding_run_error(error: RepositoryBindingVerifierError) -> RunError {
    let message = error.to_string();
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. }
        | RepositoryBindingVerifierError::Conflict { .. } => RunError::bad_request(message),
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => RunError::unavailable(message),
        RepositoryBindingVerifierError::CorruptAuthority { .. } => RunError::internal(message),
    }
}

#[cfg(test)]
#[test]
fn repository_binding_failure_controls_run_retry_semantics() {
    // Cause/effect table: malformed/denied/stale binding -> BadRequest;
    // authority outage or unknown read -> Unavailable; corrupt authority ->
    // Internal. RunError has no conflict kind, so a stale selection remains a
    // caller-correctable request failure rather than a retryable outage.
    use awaken_session_error_contract::RunErrorKind as K;
    let cases = [
        (
            RepositoryBindingVerifierError::InvalidRequest { detail: "x".into() },
            K::BadRequest,
        ),
        (
            RepositoryBindingVerifierError::Denied { detail: "x".into() },
            K::BadRequest,
        ),
        (
            RepositoryBindingVerifierError::Conflict { detail: "x".into() },
            K::BadRequest,
        ),
        (
            RepositoryBindingVerifierError::AuthorityUnavailable { detail: "x".into() },
            K::Unavailable,
        ),
        (
            RepositoryBindingVerifierError::Indeterminate { detail: "x".into() },
            K::Unavailable,
        ),
        (
            RepositoryBindingVerifierError::CorruptAuthority { detail: "x".into() },
            K::Internal,
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(repository_binding_run_error(error).kind, expected);
    }
}
