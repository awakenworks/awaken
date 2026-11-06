//! Repository binding failures mapped at the Managed resource boundary.
use awaken_resource_contract::RepositoryBindingVerifierError;
use awaken_runtime_contract::runs::RunError;

pub(in crate::resource_staging) fn repository_binding_run_error(
    error: RepositoryBindingVerifierError,
) -> RunError {
    RunError::from(crate::realization_error::RealizationError::RepositoryBinding(error))
}

#[cfg(test)]
#[test]
fn repository_binding_failure_controls_run_retry_semantics() {
    // Cause/effect table: malformed/denied binding -> BadRequest; a
    // concurrently changed authority -> Conflict (adapters answer 409; the
    // disposition retries within the budget); authority outage or unknown read
    // -> Unavailable; corrupt authority -> Internal with its own closed code,
    // so it stays terminal rather than an unknown internal fault.
    // R1..R6 below cover every verifier cause through the sole realization classifier
    // and existing Run projection. Effects include the literal public code,
    // message and Display, so consolidation cannot erase retry or text semantics.
    // This pure mapping has no IO, lock, state mutation or poison precondition.
    use awaken_runtime_contract::runs::RunErrorKind as K;
    let cases = [
        (
            RepositoryBindingVerifierError::InvalidRequest { detail: "x".into() },
            K::BadRequest,
            "invalid_request",
            "invalid Repository binding request: x",
        ),
        (
            RepositoryBindingVerifierError::Denied { detail: "x".into() },
            K::BadRequest,
            "invalid_request",
            "Repository binding was denied: x",
        ),
        (
            RepositoryBindingVerifierError::Conflict { detail: "x".into() },
            K::Conflict,
            "conflict",
            "Repository binding authority changed concurrently: x",
        ),
        (
            RepositoryBindingVerifierError::AuthorityUnavailable { detail: "x".into() },
            K::Unavailable,
            "unavailable",
            "Repository binding authority is unavailable: x",
        ),
        (
            RepositoryBindingVerifierError::Indeterminate { detail: "x".into() },
            K::Unavailable,
            "unavailable",
            "Repository binding decision is indeterminate: x",
        ),
        (
            RepositoryBindingVerifierError::CorruptAuthority { detail: "x".into() },
            K::Internal,
            "repository_binding_authority_corrupt",
            "Repository binding authority returned corrupt data: x",
        ),
    ];
    for (error, expected, code, message) in cases {
        let actual = repository_binding_run_error(error);
        assert_eq!(actual.kind, expected);
        assert_eq!(actual.code, code);
        assert_eq!(actual.message, message);
        assert_eq!(actual.to_string(), format!("run failed: {message}"));
    }
}
