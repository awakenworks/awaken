//! Integration tests for the dispatch-owned claim laws.
#[cfg(test)]
mod tests {

    #[test]
    fn host_resolution_projects_the_contract_class_like_the_driver() {
        // Cause/effect table: an executor (Host) failure reaches the claim
        // resolver through its neutral Run projection and the contract's one
        // classification, exactly as the realization driver classifies the
        // same failure after the synchronizer. H1 BadRequest, H3 unclassified
        // Internal and H4 Conflict (projected to BadRequest) absorb; H2
        // Unavailable relinquishes. Every row uses identical detail text so an
        // operator message cannot become a second classification policy.
        for (error, terminal) in [
            (crate::HostError::bad_request("same detail"), true),
            (crate::HostError::unavailable("same detail"), false),
            (crate::HostError::internal("same detail"), true),
            (crate::HostError::conflict("same detail"), true),
        ] {
            let run_error = crate::host_error::to_run_error(crate::HostError {
                message: error.message.clone(),
                kind: error.kind,
                code: error.code.clone(),
            });
            let actual =
                awaken_dispatch_execution::session_worker_resolver::map_admission_error(error);
            assert_eq!(actual.is_terminal_resolution(), terminal);
            let driver = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(run_error),
            );
            assert_eq!(driver.is_terminal_resolution(), terminal, "driver agrees");
            if !terminal {
                assert!(matches!(
                    actual,
                    awaken_dispatch_execution::Error::Execution(_)
                ));
            }
        }
    }

    fn claim_outcome(error: &awaken_dispatch_execution::Error) -> &'static str {
        match error {
            awaken_dispatch_execution::Error::ResolutionNotReady(_) => "defer",
            awaken_dispatch_execution::Error::Execution(_) => "relinquish",
            awaken_dispatch_execution::Error::TerminalResolution(_) => "absorb",
            other => panic!("unexpected claim outcome {other:?}"),
        }
    }

    #[test]
    fn sandbox_failure_class_reaches_the_same_claim_outcome_on_both_paths() {
        // End-to-end decision table (ADR-0092 step 0): SandboxError class x
        // path -> claim outcome and Session state. Causes: C1 the provider's
        // SandboxError variant (its Classify disposition, or the capacity
        // class); C2 the path that observes it: P1 the realization driver
        // (synchronizer -> RunError -> FailSessionRealization + drive error)
        // or P2 the claim resolver (HostError -> map_admission_error).
        // Effects: the claim outcome (the dispatch pool defers every
        // non-terminal outcome request-locally with the claim's backoff and
        // keeps its retry budget; `absorb` settles the Run), and the
        // FailSessionRealization report, which decides the Session state
        // (session-application `failure_budget` table):
        // | Rule | class | claim (P1 = P2) | report (retryable, capacity) | Session state | Session budget consumed? | dispatch budget consumed? |
        // | X1 | Transient: NotReady/Transport/Timeout/Backend | relinquish (request-local defer) | (true, false) | Preparing; ActivationFailed once the budget is spent | yes, one attempt | no |
        // | X2 | CapacityUnavailable | defer (request-local) | (true, true) | Preparing, never ActivationFailed | no, attempt refunded | no |
        // | X3 | Permanent: NotFound/Gone/Unauthorized/FenceRejected/Invalid/Unsupported/Conflict | absorb (TerminalResolution) | (false, false) | ActivationFailed | n/a (absorbing) | n/a (Run settled) |
        // Session-state and budget columns are proven on the durable aggregate by
        // session-application `capacity_budget` and the contract's
        // `capacity_failures_retry_without_consuming_and_others_keep_the_budget`;
        // the dispatch column by the pool's ClaimDeferred path (defer_claim keeps
        // attempt_count, run_ingress obligations).
        use awaken_provisioning_contract::SandboxError;
        let rows = [
            (
                "X1",
                SandboxError::not_ready("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::transport("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::timeout("d"),
                "relinquish",
                (true, false),
            ),
            (
                "X1",
                SandboxError::backend("op", "d"),
                "relinquish",
                (true, false),
            ),
            (
                "X2",
                SandboxError::capacity_unavailable("exceeded quota"),
                "defer",
                (true, true),
            ),
            ("X3", SandboxError::not_found("d"), "absorb", (false, false)),
            ("X3", SandboxError::gone("d"), "absorb", (false, false)),
            (
                "X3",
                SandboxError::unauthorized("d"),
                "absorb",
                (false, false),
            ),
            (
                "X3",
                SandboxError::fence_rejected("d"),
                "absorb",
                (false, false),
            ),
            ("X3", SandboxError::invalid("d"), "absorb", (false, false)),
            (
                "X3",
                SandboxError::unsupported("d"),
                "absorb",
                (false, false),
            ),
            ("X3", SandboxError::conflict("d"), "absorb", (false, false)),
        ];
        for (rule, sandbox, claim, report) in rows {
            // P2: the resolver sees the Host error directly.
            let host_error = || {
                crate::host_error::from_run_error(
                    awaken_session_realization_contract::environment_effect_error(&sandbox),
                )
            };
            let resolver = awaken_dispatch_execution::session_worker_resolver::map_admission_error(
                host_error(),
            );
            // P1: the driver sees the same Host error after the synchronizer's
            // Run projection, reports it to Control, then returns it.
            let run_error = crate::host_error::to_run_error(host_error());
            let command = awaken_session_realization_contract::FailSessionRealization::for_effect(
                "s".into(),
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    owner: "w".into(),
                    runtime_incarnation: "w/1".into(),
                    epoch: 1,
                    expires_at_unix_ms: 1,
                },
                None,
                None,
                &run_error,
            );
            let reported = (command.retryable, command.capacity_unavailable);
            let driver = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    run_error,
                ),
            );
            assert_eq!(claim_outcome(&resolver), claim, "{rule} P2 {sandbox}");
            assert_eq!(claim_outcome(&driver), claim, "{rule} P1 {sandbox}");
            assert_eq!(reported, report, "{rule} report {sandbox}");
            assert!(
                resolver.to_string().contains(&sandbox.to_string()),
                "{rule}: the provider detail survives the boundary"
            );
        }
        // Fail closed: an unclassified Host failure absorbs on the resolver as
        // it does on the driver, and a capacity code on a non-Unavailable fault
        // is not a capacity refusal.
        assert_eq!(
            claim_outcome(
                &awaken_dispatch_execution::session_worker_resolver::map_admission_error(
                    crate::HostError::internal("unclassified")
                )
            ),
            "absorb"
        );
        assert_eq!(
            claim_outcome(&awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::classified(
                        awaken_session_realization_contract::ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                        "mislabelled",
                    ),
                ),
            )),
            "absorb"
        );
    }

    #[test]
    fn session_realization_preserves_retryable_and_absorbing_failure_classes() {
        // Cause/effect graph: C1 the canonical driver classifies an effect as
        // Unavailable, C2 Control reports transient ownership/readiness, or C3
        // the phase is permanently invalid. Effects: C1/C2 relinquish or defer
        // the WorkQueue claim; only C3 becomes TerminalResolution. This preserves
        // the Session aggregate's already-persisted retry decision instead of
        // creating a second host-side policy.
        //
        // | Rule | driver cause | effect |
        // | R1 | unavailable effect | retryable Execution |
        // | R2 | Control unavailable/conflict | retryable Execution |
        // | R3 | Control not ready | ResolutionNotReady |
        // | R4 | permanent effect/invalid Control | TerminalResolution |
        let retryable_effect =
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::unavailable_classified(
                        "mcp_material_source_unavailable",
                        "materializer unavailable",
                    ),
                ),
            );
        assert!(matches!(
            retryable_effect,
            awaken_dispatch_execution::Error::Execution(_)
        ));

        let retryable_control = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "repository unavailable".into(),
                ),
            ),
        );
        assert!(matches!(
            retryable_control,
            awaken_dispatch_execution::Error::Execution(_)
        ));

        let not_ready =
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Control(
                    awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
                ),
            );
        assert!(matches!(
            not_ready,
            awaken_dispatch_execution::Error::ResolutionNotReady(_)
        ));

        for permanent in [
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                awaken_session_types::RunError::classified(
                    "mcp_credential_revision_mismatch",
                    "invalid revision",
                ),
            ),
            awaken_session_realization_contract::SessionRealizationDriveError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    "invalid phase".into(),
                ),
            ),
        ] {
            assert!(matches!(
                awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(permanent),
                awaken_dispatch_execution::Error::TerminalResolution(_)
            ));
        }
    }

    use awaken_dispatch_execution::session_worker_resolver::claim_failure::session_realization_worker_effect;
    use awaken_dispatch_execution::session_worker_resolver::{
        map_admission_error, map_session_realization_drive_error,
    };
    use awaken_provisioning_contract::SandboxError;
    use awaken_session_realization::realization_error::{
        AuthorityLoss, PendingState, ProjectionConflict, RealizationError, RealizationLock,
    };

    /// One table row: rule, the failure it builds, and the expected outcome.
    type Row = (&'static str, fn() -> RealizationError, &'static str);

    fn outcome(error: &awaken_dispatch_execution::Error) -> &'static str {
        match error {
            awaken_dispatch_execution::Error::ResolutionNotReady(_) => "defer",
            awaken_dispatch_execution::Error::Execution(_) => "relinquish",
            awaken_dispatch_execution::Error::TerminalResolution(_) => "absorb",
            other => panic!("unexpected claim outcome {other:?}"),
        }
    }

    #[test]
    fn one_realization_failure_reaches_one_outcome_on_every_path() {
        // Decision table: cause = the realization failure class; paths = P1
        // the claim resolver (RealizationError -> AdmissionError -> claim),
        // P2 the projection synchronizer (its one Run projection) and P3 the
        // realization driver (the synchronizer's Run error as a drive error).
        // Effect: the claim outcome, identical on all three paths.
        // | Class                                   | outcome    |
        // | transient provider / authority / pending / poisoned / stale Run | relinquish |
        // | provider capacity                       | defer      |
        // | permanent provider / projection conflict / invariant / invalid | absorb |
        let rows: [Row; 10] = [
            (
                "transient provider",
                || SandboxError::timeout("t").into(),
                "relinquish",
            ),
            (
                "capacity",
                || SandboxError::capacity_unavailable("q").into(),
                "defer",
            ),
            (
                "permanent provider",
                || SandboxError::unauthorized("u").into(),
                "absorb",
            ),
            (
                "authority lost",
                || RealizationError::authority_lost(AuthorityLoss::ClaimStale, "stale claim"),
                "relinquish",
            ),
            (
                "pending",
                || RealizationError::pending(PendingState::TerminalHandNotQuiescent, "busy"),
                "relinquish",
            ),
            (
                "projection conflict",
                || {
                    RealizationError::conflict(
                        ProjectionConflict::PublicationConflict,
                        "foreign publication",
                    )
                },
                "absorb",
            ),
            (
                "invariant",
                || RealizationError::invariant("broken"),
                "absorb",
            ),
            (
                "invalid request",
                || RealizationError::invalid_request("bad"),
                "absorb",
            ),
            (
                "poisoned registry",
                || RealizationError::Poisoned(RealizationLock::InstalledSessions),
                "relinquish",
            ),
            (
                "stale Run lease",
                || {
                    awaken_session_types::RunError::unavailable_classified(
                        "session_realization_stale",
                        "stale",
                    )
                    .into()
                },
                "relinquish",
            ),
        ];
        for (rule, failure, expected) in rows {
            let claim =
                map_admission_error(crate::host::dispatch_execution::admission_error(failure()));
            let synchronizer = awaken_session_types::RunError::from(failure());
            let driver = map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::from(failure()),
                ),
            );
            assert_eq!(outcome(&claim), expected, "{rule} P1 claim");
            assert_eq!(
                session_realization_worker_effect(
                    awaken_session_realization_contract::effect_failure_disposition(&synchronizer)
                ),
                session_realization_worker_effect(failure().disposition().contract_class()),
                "{rule} P2 synchronizer"
            );
            assert_eq!(outcome(&driver), expected, "{rule} P3 driver");
        }
    }

    #[test]
    fn realization_failures_keep_their_claim_class_through_the_admission_mapping() {
        // Decision table (moved with the realization's entry into dispatch):
        // cause = the realization failure; effect = the claim class of the
        // AdmissionError the executor's one mapping produces. The class is
        // the realization disposition's contract class.
        use awaken_session_realization_contract::SessionRealizationControlDisposition::{
            NotReady, Retryable, Terminal,
        };
        let rows = [
            (
                "A7 realization authority lost",
                RealizationError::authority_lost(AuthorityLoss::ClaimStale, "stale"),
                Retryable,
            ),
            (
                "A7 realization pending",
                RealizationError::pending(PendingState::TerminalHandNotQuiescent, "busy"),
                Retryable,
            ),
            (
                "A7 realization capacity",
                RealizationError::Environment(SandboxError::capacity_unavailable("quota")),
                NotReady,
            ),
            (
                "A7 realization invariant",
                RealizationError::invariant("broken"),
                Terminal,
            ),
        ];
        for (rule, error, expected) in rows {
            assert_eq!(
                error.disposition().contract_class(),
                expected,
                "{rule} disposition"
            );
            assert_eq!(
                crate::host::dispatch_execution::admission_error(error).claim_disposition(),
                expected,
                "{rule}"
            );
        }
    }
}
