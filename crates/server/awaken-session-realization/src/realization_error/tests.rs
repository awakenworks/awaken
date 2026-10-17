use super::*;
use awaken_session_realization_contract::FailSessionRealization;

fn lease() -> awaken_session_configuration_contract::resource::SessionRealizationLease {
    awaken_session_configuration_contract::resource::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    }
}

/// Every realization failure class with the disposition its fault implies.
/// The oracle is written per row from the fault, never from `disposition()`.
fn decision_table() -> Vec<(&'static str, RealizationError, Disposition)> {
    let mut rows = vec![
        (
            "D1 transient provider failure",
            RealizationError::Environment(SandboxError::timeout("t")),
            Disposition::Retry,
        ),
        (
            "D1 transitional provider state",
            RealizationError::Environment(SandboxError::not_ready("n")),
            Disposition::Retry,
        ),
        (
            "D2 provider capacity refusal",
            RealizationError::Environment(SandboxError::capacity_unavailable("quota")),
            Disposition::Defer,
        ),
        (
            "D3 permanent provider failure",
            RealizationError::Environment(SandboxError::unauthorized("u")),
            Disposition::Terminal,
        ),
        (
            "D3 provider conflict",
            RealizationError::Environment(SandboxError::conflict("c")),
            Disposition::Terminal,
        ),
        (
            "D7 stale Run lease from a contract port",
            RealizationError::Run(RunError::unavailable_classified(
                "session_realization_stale",
                "stale",
            )),
            Disposition::Retry,
        ),
        (
            "D7 Run capacity refusal",
            RealizationError::Run(RunError::unavailable_classified(
                ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                "quota",
            )),
            Disposition::Defer,
        ),
        (
            "D7 internal Run failure",
            RealizationError::Run(RunError::classified("custom", "broken")),
            Disposition::Terminal,
        ),
        (
            "D7 invalid Run request",
            RealizationError::Run(RunError::bad_request("bad")),
            Disposition::Rejected,
        ),
        (
            "D8 invalid realization request",
            RealizationError::invalid_request("trusted_host is disabled"),
            Disposition::Rejected,
        ),
        (
            "D9 broken realization invariant",
            RealizationError::invariant("Candidate lost its Environment"),
            Disposition::Terminal,
        ),
    ];
    for cause in AuthorityLoss::ALL {
        rows.push((
            "D4 authority lost",
            RealizationError::authority_lost(*cause, "lost"),
            Disposition::Retry,
        ));
    }
    for state in PendingState::ALL {
        rows.push((
            "D5 pending state",
            RealizationError::pending(*state, "pending"),
            Disposition::Retry,
        ));
    }
    for conflict in ProjectionConflict::ALL {
        rows.push((
            "D6 projection conflict",
            RealizationError::conflict(*conflict, "conflict"),
            Disposition::Terminal,
        ));
    }
    for (error, disposition) in [
        (
            RepositoryBindingVerifierError::InvalidRequest { detail: "x".into() },
            Disposition::Rejected,
        ),
        (
            RepositoryBindingVerifierError::Denied { detail: "x".into() },
            Disposition::Rejected,
        ),
        (
            RepositoryBindingVerifierError::Conflict { detail: "x".into() },
            Disposition::Rejected,
        ),
        (
            RepositoryBindingVerifierError::AuthorityUnavailable { detail: "x".into() },
            Disposition::Retry,
        ),
        (
            RepositoryBindingVerifierError::Indeterminate { detail: "x".into() },
            Disposition::Retry,
        ),
        (
            RepositoryBindingVerifierError::CorruptAuthority { detail: "x".into() },
            Disposition::Terminal,
        ),
    ] {
        rows.push((
            "D10 Repository binding",
            RealizationError::RepositoryBinding(error),
            disposition,
        ));
    }
    for lock in [
        RealizationLock::InstalledSessions,
        RealizationLock::EnvironmentBindingSink,
        RealizationLock::MemoryExecutionContext,
    ] {
        rows.push((
            "D11 poisoned lock denies",
            RealizationError::Poisoned(lock),
            Disposition::Retry,
        ));
    }
    for (error, disposition) in [
        (
            awaken_resource_contract::FileContentSourceError::Indeterminate { detail: "x".into() },
            Disposition::Retry,
        ),
        (
            awaken_resource_contract::FileContentSourceError::Integrity {
                file_id: "f".into(),
                expected: "a".into(),
                actual: "b".into(),
            },
            Disposition::Terminal,
        ),
    ] {
        rows.push((
            "D12 File content",
            RealizationError::FileContent(error),
            disposition,
        ));
    }
    rows.push((
        "D13 malformed evidence",
        RealizationError::from(awaken_session_state_contract::SessionCleanupError::ReceiptMismatch),
        Disposition::Terminal,
    ));
    rows.push((
        "D4 receipt fenced by another owner",
        RealizationError::from(
            awaken_session_state_contract::SessionEnvironmentReceiptError::RealizationStale,
        ),
        Disposition::Retry,
    ));
    rows
}

#[test]
fn each_failure_class_has_one_disposition_on_the_synchronizer_and_driver_paths() {
    // Decision table: cause = the realization failure class (D1-D11 above);
    // paths = P1 the realization's own decision, P2 the projection
    // synchronizer (RealizationError -> RunError, classified by the
    // contract), P3 the realization driver (FailSessionRealization report and
    // the drive error's Worker class). Effect: one disposition per class on
    // every path, and the projected code is the error's own closed code.
    // | Class | P1 = P2 = P3 |
    // | D1 transient provider / D4 authority lost / D5 pending / D11 poisoned | Retry |
    // | D2 capacity | Defer (report keeps the budget) |
    // | D3 permanent provider / D6 conflict / D9 invariant | Terminal |
    // | D7 Run port failure | the Run error's own class |
    // | D8 invalid request | Rejected (absorbs like Terminal) |
    // | D10 Repository binding | by verifier class |
    for (rule, error, expected) in decision_table() {
        let code = error.code().into_owned();
        assert_eq!(error.disposition(), expected, "{rule} P1 {error}");
        let projected = RunError::from(error);
        assert_eq!(projected.code, code, "{rule} code survives projection");
        assert_eq!(Disposition::of_run_error(&projected), expected, "{rule} P2");
        assert_eq!(
            effect_failure_disposition(&projected),
            expected.contract_class(),
            "{rule} P3 drive error class"
        );
        let report =
            FailSessionRealization::for_effect("s".into(), lease(), None, None, &projected);
        assert_eq!(
            (report.retryable, report.capacity_unavailable),
            match expected {
                Disposition::Retry => (true, false),
                Disposition::Defer => (true, true),
                Disposition::Terminal | Disposition::Rejected => (false, false),
            },
            "{rule} P3 report"
        );
    }
}

#[test]
fn closed_codes_are_unique_across_classes() {
    // Each code names exactly one fault class: the same code under two classes
    // would let a protocol adapter disagree with `disposition()`.
    let mut seen = std::collections::HashSet::new();
    let codes = AuthorityLoss::ALL
        .iter()
        .map(|cause| cause.code())
        .chain(PendingState::ALL.iter().map(|state| state.code()))
        .chain(
            ProjectionConflict::ALL
                .iter()
                .map(|conflict| conflict.code()),
        )
        .chain([POISONED_CODE]);
    for code in codes {
        assert!(seen.insert(code), "duplicate realization code {code}");
    }
}

#[test]
fn disposition_maps_to_the_contract_worker_class() {
    // Rejected and Terminal both absorb; Defer is the contract's NotReady.
    for (disposition, control) in [
        (
            Disposition::Retry,
            SessionRealizationControlDisposition::Retryable,
        ),
        (
            Disposition::Defer,
            SessionRealizationControlDisposition::NotReady,
        ),
        (
            Disposition::Terminal,
            SessionRealizationControlDisposition::Terminal,
        ),
        (
            Disposition::Rejected,
            SessionRealizationControlDisposition::Terminal,
        ),
    ] {
        assert_eq!(disposition.contract_class(), control, "{disposition:?}");
    }
}

#[test]
fn projection_keeps_the_operator_message() {
    let error = RealizationError::pending(PendingState::TerminalHandNotQuiescent, "hand busy");
    assert_eq!(error.to_string(), "hand busy");
    assert_eq!(RunError::from(error).message, "hand busy");
    let provider = SandboxError::timeout("slow");
    assert_eq!(
        RunError::from(RealizationError::Environment(provider.clone())).message,
        provider.to_string()
    );
}
