use super::*;
use awaken_session_realization_contract::FailSessionRealization;

#[test]
fn physical_observation_refusal_preserves_existing_codes_and_operator_text() {
    // PC1 decision table: two existing caller-owned conflict codes crossed
    // with Incompatible and retention-only facts. Both reject physical effects;
    // Incompatible preserves its exact previous text, while retention names the
    // unexpected fact. PC2 lifecycle observation I/O retains its original
    // pending classification and diagnostic text after duplicate consolidation.
    for conflict in [
        ProjectionConflict::EffectIncompatible,
        ProjectionConflict::ObservationIncompatible,
    ] {
        for observation in [
            awaken_provisioning_contract::SandboxObservation::Incompatible {
                reason: "original incompatibility".into(),
            },
            awaken_provisioning_contract::SandboxObservation::RetainedOriginal {
                physical_incarnation: "original-pod".into(),
            },
        ] {
            let error = RealizationError::physical_observation_conflict(conflict, &observation);
            match error {
                RealizationError::ProjectionConflict {
                    conflict: actual,
                    message,
                } => {
                    assert_eq!(actual, conflict);
                    if matches!(
                        observation,
                        awaken_provisioning_contract::SandboxObservation::Incompatible { .. }
                    ) {
                        assert_eq!(message, "original incompatibility");
                    } else {
                        assert!(message.contains("RetainedOriginal"));
                    }
                }
                other => panic!("PC1 physical conflict lost: {other:?}"),
            }
        }
    }
    match RealizationError::observation_pending("source", SandboxError::timeout("deadline")) {
        RealizationError::Pending { state, message } => {
            assert_eq!(state, PendingState::ObservationIndeterminate);
            assert_eq!(
                message,
                "could not inspect Session sandbox source: sandbox error: deadline"
            );
        }
        other => panic!("PC2 observation failure lost: {other:?}"),
    }
}

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
            "D7 classified internal Run failure",
            RealizationError::Run(RunError::classified("custom", "broken")),
            Disposition::Terminal,
        ),
        (
            "D7 unclassified internal Run failure",
            RealizationError::Run(RunError::internal("unknown")),
            Disposition::Retry,
        ),
        (
            "D7 Run state conflict",
            RealizationError::Run(RunError::conflict("raced")),
            Disposition::Retry,
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
    for (rule, deadline, disposition) in [
        (
            "D14 provisioning with no deadline",
            None,
            Disposition::Retry,
        ),
        (
            "D14 provisioning before its deadline",
            Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                10_000, 4_000,
            )),
            Disposition::Defer,
        ),
        (
            "D14 provisioning at its deadline",
            Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                10_000, 10_000,
            )),
            Disposition::Retry,
        ),
        (
            "D14 provisioning past its deadline",
            Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                10_000, 70_000,
            )),
            Disposition::Retry,
        ),
    ] {
        rows.push((
            rule,
            RealizationError::provisioning(deadline, "provisioning"),
            disposition,
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
            Disposition::Retry,
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
    // Memory's context lock moved with its sole binding into Run delivery;
    // memory_commit_binding_preserves_identity_and_refuses_poisoned_reads_and_writes
    // exercises the real lock and preserves this row's original code/class there.
    for lock in [
        RealizationLock::InstalledSessions,
        RealizationLock::EnvironmentBindingSink,
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
    // D15: an aggregate refusal is classified by its own recovery action, not
    // by its name. The oracle is the contract's documented class per row.
    {
        use awaken_session_state_contract::SessionEnvironmentReceiptError as Receipt;
        use awaken_session_state_contract::SessionEnvironmentRejection as Rejection;
        use awaken_session_state_contract::SessionEnvironmentTransitionError as Transition;
        for (rule, error, disposition) in [
            (
                "D15 receipt fenced by another owner retries",
                RealizationError::from(Receipt::RealizationStale),
                Disposition::Retry,
            ),
            (
                "D15 pending Resource transition retries",
                RealizationError::from(Receipt::ResourceTransitionMismatch),
                Disposition::Retry,
            ),
            (
                "D15 unknown original materialization retries",
                RealizationError::from(Receipt::MaterializationUnknown),
                Disposition::Retry,
            ),
            (
                "D15 receipt step refusal that may clear retries",
                RealizationError::from(Receipt::Rejected(Rejection::ContinuationInProgress)),
                Disposition::Retry,
            ),
            (
                "D15 law step refusal that may clear retries",
                RealizationError::from(Transition::Rejected(Rejection::LeaseNotCurrent)),
                Disposition::Retry,
            ),
            (
                "D15 mismatched receipt is final",
                RealizationError::from(Receipt::Mismatch),
                Disposition::Terminal,
            ),
            (
                "D15 cross-Environment receipt is final",
                RealizationError::from(Receipt::EnvironmentMismatch),
                Disposition::Terminal,
            ),
            (
                "D15 receipt causal refusal is final",
                RealizationError::from(Receipt::Rejected(Rejection::InvalidTransition)),
                Disposition::Terminal,
            ),
            (
                "D15 law evidence refusal is final",
                RealizationError::from(Transition::CheckpointExpired),
                Disposition::Terminal,
            ),
            (
                "D15 law causal refusal is final",
                RealizationError::from(Transition::Rejected(Rejection::InconsistentEvidence)),
                Disposition::Terminal,
            ),
        ] {
            rows.push((rule, error, disposition));
        }
    }
    rows
}

#[test]
fn an_environment_refusal_reads_its_own_recovery_classification() {
    // Cause/effect table: for every receipt and law refusal the contract
    // classifies, the realization disposition is Retry exactly when the
    // refusal's own recovery action is Retry, and Terminal when it is Reject.
    // No refusal is told apart by its variant name.
    use awaken_session_state_contract::SessionEnvironmentReceiptError as Receipt;
    use awaken_session_state_contract::SessionEnvironmentRejection as Rejection;
    use awaken_session_state_contract::SessionEnvironmentTransitionError as Transition;
    let rejections = [
        Rejection::Terminal,
        Rejection::NotTerminal,
        Rejection::ContinuationInProgress,
        Rejection::ActivitiesOpen,
        Rejection::NotGenerated,
        Rejection::WrongStep,
        Rejection::RequiresResident,
        Rejection::InvalidTransition,
        Rejection::LeaseNotCurrent,
        Rejection::GenerationExpired,
        Rejection::CheckpointLive,
        Rejection::InconsistentEvidence,
        Rejection::MaterializationPending,
        Rejection::UnsupportedContinuationTopology,
        Rejection::MemoryEvidenceUnproven,
    ];
    let mut refusals: Vec<EnvironmentRefusal> = vec![
        Receipt::Mismatch.into(),
        Receipt::EnvironmentMismatch.into(),
        Receipt::RealizationStale.into(),
        Receipt::InvalidReservation.into(),
        Receipt::ResourceTransitionMismatch.into(),
        Receipt::MaterializationUnknown.into(),
        Transition::ReceiptMismatch.into(),
        Transition::CheckpointExpired.into(),
        Transition::InvalidCheckpointPolicy("policy").into(),
    ];
    for rejection in rejections {
        refusals.push(Receipt::Rejected(rejection).into());
        refusals.push(Transition::Rejected(rejection).into());
    }
    for refusal in refusals {
        let expected = match refusal.recovery_action() {
            SessionEnvironmentReceiptRecoveryAction::Retry => Disposition::Retry,
            SessionEnvironmentReceiptRecoveryAction::Reject => Disposition::Terminal,
        };
        let label = refusal.to_string();
        assert_eq!(
            RealizationError::EnvironmentRefused(refusal).disposition(),
            expected,
            "{label}"
        );
    }
}

#[test]
fn each_failure_class_has_one_disposition_on_the_synchronizer_and_driver_paths() {
    // Decision table: cause = the realization failure class (D1-D15 above);
    // paths = P1 the realization's own decision, P2 the projection
    // synchronizer (RealizationError -> RunError, classified by the
    // contract), P3 the realization driver (FailSessionRealization report and
    // the drive error's Worker class). Effect: one disposition per class on
    // every path, and the projected code is the error's own closed code.
    // Environment rows D1-D3 use the existing contract projection on every
    // path: capacity, transient and permanent failures cannot acquire a
    // second table when the Provider routing vocabulary gains a variant.
    // | Class | P1 = P2 = P3 |
    // | D1 transient provider / D4 authority lost / D5 pending / D11 poisoned | Retry |
    // | D2 capacity | Defer (report keeps the budget) |
    // | D14 provisioning before the provider's deadline | Defer (report keeps the budget) |
    // | D14 provisioning with no deadline, at or past it | Retry |
    // | D3 permanent provider / D6 projection conflict / D9 invariant / D13 evidence | Terminal |
    // | D15 aggregate refusal | its own recovery action: Retry retries, Reject is Terminal |
    // | D7 Run port failure | the Run error's own class: unavailable, state conflict and unclassified internal retry; classified internal is terminal |
    // | D8 invalid request | Rejected (absorbs like Terminal) |
    // | D10 Repository binding | by verifier class; a concurrent authority change retries |
    // Retry consumes one attempt of the caller's budget (the report is
    // retryable and not a capacity refusal); its exhaustion is the contract's
    // `conflict_and_unclassified_internal_retry_until_the_realization_budget_is_spent`
    // and, on the claim, the dispatch retry budget (`retry_claim`, ADR-0015).
    // D5 pending stays Retry, not Defer: Defer is only for a wait whose owner
    // guarantees it resolves or expires, and no owner holds a deadline over
    // these states. They are observations of provider or evidence state (a
    // failed or indeterminate observation, a closed candidate). A provider
    // that keeps failing to answer never settles on its own; the realization
    // lease expires for the claimant, not for the observed sandbox.
    // D14 is the one wait with an owner: a provider whose backend progresses
    // provisioning on its own (the cluster for a Pod) gives the observation
    // its deadline, measured from the backend's durable creation record.
    // Before it the wait defers; from it on, or without one (Docker, Podman
    // and local sandboxes are progressed only by their creating call, and a
    // legacy observation carries none), it is a failed attempt. It is Retry,
    // not Terminal: the next attempt may find the sandbox ready or replaced,
    // and the dispatch budget still bounds it.
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
            (report.retryable, report.not_ready),
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
        .chain(UnrecoverableFault::ALL.iter().map(|fault| fault.code()))
        .chain([
            POISONED_CODE,
            REPOSITORY_BINDING_CORRUPT_CODE,
            PROVISIONING_UNBOUNDED_CODE,
            PROVISIONING_DEADLINE_ELAPSED_CODE,
            ENVIRONMENT_PROVISIONING_CODE,
        ]);
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

#[test]
fn a_terminal_failure_never_projects_the_unclassified_internal_code() {
    // Cause: a terminal realization failure. Effect: its Run projection names
    // its own closed code, so the contract cannot read it as an unknown
    // internal fault and retry it.
    for (rule, error, expected) in decision_table() {
        if expected == Disposition::Terminal {
            let projected = RunError::from(error);
            assert!(!projected.is_unclassified_internal(), "{rule}");
        }
    }
}

#[test]
fn a_repository_binding_conflict_projects_a_run_conflict() {
    // Cause: the binding authority changed concurrently. Effects: the Run
    // projection is a state conflict (so adapters answer 409 conflict) and the
    // disposition retries.
    let error = RealizationError::RepositoryBinding(RepositoryBindingVerifierError::Conflict {
        detail: "x".into(),
    });
    assert_eq!(error.disposition(), Disposition::Retry);
    let projected = RunError::from(error);
    assert_eq!(projected.kind, awaken_session_types::RunErrorKind::Conflict);
    assert_eq!(projected.code, "conflict");
    assert_eq!(
        projected.message,
        "Repository binding authority changed concurrently: x"
    );
}

#[test]
fn only_a_provision_within_its_deadline_hints_when_to_look_again() {
    // Decision table: cause = the failure; effect = the retry-after hint.
    // | H1 | provisioning before its deadline | the wait left to the deadline |
    // | H2 | provisioning at / past / without a deadline | none |
    // | H3 | a capacity refusal and every other class | none (caller's backoff) |
    use awaken_provisioning_contract::ProvisioningDeadline;
    use std::time::Duration;
    for (rule, error, hint) in [
        (
            "H1",
            RealizationError::provisioning(Some(ProvisioningDeadline::new(10_000, 4_000)), "p"),
            Some(Duration::from_millis(6_000)),
        ),
        (
            "H2",
            RealizationError::provisioning(Some(ProvisioningDeadline::new(10_000, 10_000)), "p"),
            None,
        ),
        ("H2", RealizationError::provisioning(None, "p"), None),
        (
            "H3",
            RealizationError::Environment(SandboxError::capacity_unavailable("quota")),
            None,
        ),
        (
            "H3",
            RealizationError::pending(PendingState::ObservationIndeterminate, "unknown"),
            None,
        ),
    ] {
        assert_eq!(error.retry_after(), hint, "{rule}");
        if let Some(hint) = hint {
            assert_eq!(
                error.disposition(),
                Disposition::Defer,
                "{rule} hint only on Defer"
            );
            assert!(!hint.is_zero(), "{rule}");
        }
        // H4 all H1-H3 observations cross the projection/driver boundary
        // through the same Run projection, preserving this exact provider
        // hint instead of classifying or sampling its deadline a second time.
        let class = error.disposition().contract_class();
        let code = error.code().into_owned();
        let failure =
            awaken_session_realization_contract::SessionRealizationEffectFailure::from(error);
        assert_eq!(failure.retry_after(), hint, "H4 {rule} hint");
        assert_eq!(failure.run_error().code, code, "H4 {rule} code");
        assert_eq!(
            awaken_session_realization_contract::effect_failure_disposition(failure.run_error()),
            class,
            "H4 {rule} class"
        );
    }
}
