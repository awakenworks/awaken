//! Integration tests for the dispatch-owned claim laws.
#[cfg(test)]
mod tests {

    #[test]
    fn host_resolution_projects_the_contract_class_like_the_driver() {
        // Cause/effect table: an executor (Host) failure reaches the claim
        // resolver through its neutral Run projection and the contract's one
        // classification, exactly as the realization driver classifies the
        // same failure after the synchronizer. H1 BadRequest and H5 classified
        // Internal absorb; H2 Unavailable, H3 unclassified Internal and H4
        // Conflict relinquish within the retry budget. Every row uses identical
        // detail text so an operator message cannot become a second
        // classification policy.
        for (error, terminal) in [
            (
                crate::SessionExecutionError::bad_request("same detail"),
                true,
            ),
            (
                crate::SessionExecutionError::unavailable("same detail"),
                false,
            ),
            (crate::SessionExecutionError::internal("same detail"), false),
            (crate::SessionExecutionError::conflict("same detail"), false),
            (
                crate::SessionExecutionError::classified(
                    "session_realization_invariant_violated",
                    "same detail",
                ),
                true,
            ),
        ] {
            let run_error =
                crate::session_execution_error::to_run_error(crate::SessionExecutionError {
                    message: error.message.clone(),
                    kind: error.kind,
                    code: error.code.clone(),
                });
            let actual =
                awaken_dispatch_execution::session_worker_resolver::map_admission_error(error);
            assert_eq!(actual.is_terminal_resolution(), terminal);
            let driver = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(run_error.into()),
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

    /// Cause/effect row C3: an ordinary SQLite Queue claim carries exact Runtime,
    /// Resources and publication, but the real Provider Create refuses capacity
    /// before allocating a handle. The original resolver returns Defer/None;
    /// its exact claim stays current and no dispatch retry attempt is consumed.
    /// This is an ordinary/unowned Run, not a Managed failure/refund CAS proof.
    /// No installed environment, binding or cleanup success may be invented.
    /// The backend conformance deferral table owns later settlement/handoff.
    #[tokio::test]
    async fn real_provider_create_capacity_refusal_defers_the_original_ordinary_claim() {
        use crate::host::worker_resolver::test_support::{
            adoption_dependencies, eager_environment, test_activation, with_empty_session_resources,
        };
        use awaken_dispatch_contract::{
            DispatchClaimOwnership as _, DispatchClaims as _, DispatchEnqueue as _,
            DispatchReads as _, DispatchState, RunClaim, RunDispatch, SessionRuntimeEnvelope,
        };
        use awaken_dispatch::WorkerResolver as _;
        use awaken_session_environment::SandboxEnvironmentProvider;
        use awaken_session_environment::test_support::{
            FakeContainerProvider, FakeHandExecutorFactory,
        };
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let fake = Arc::new(FakeContainerProvider::default());
        *fake.create_error.lock().unwrap() = Some(
            awaken_provisioning_contract::SandboxError::capacity_unavailable("provider quota"),
        );
        let provider = SandboxEnvironmentProvider::container(
            fake.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "awaken-sandbox",
        )
        .into_session_provider();
        let host = adoption_dependencies()
            .await
            .with_session_environment_provider(provider)
            .with_ephemeral_dispatch(store.clone())
            .build()
            .unwrap();
        let resolver =
            crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch).unwrap();
        let runtime = SessionRuntimeEnvelope::new(
            awaken_session_configuration_contract::SessionRuntimeProjection {
                environment: eager_environment(),
                tools: Some(Default::default()),
                mcp_stages: Some(Vec::new()),
            }
            .encode()
            .unwrap(),
        );
        store
            .enqueue(with_empty_session_resources(
                RunDispatch::new(test_activation("session-container", "capacity-create-run"))
                    .with_session_runtime(runtime),
                host.run_assembly.local_workspace(),
            ))
            .await
            .unwrap();
        let attempts_before = store.list_dispatches().await.unwrap()[0].attempt_count;
        clock.set(awaken_time::system_clock().now().as_epoch_ms());
        let claimed = store
            .claim("capacity-worker", 60_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        assert!(claimed.request.session_thread_id.is_none());
        assert!(claimed.session_work_authority.is_none());
        let claim = RunClaim::from(&claimed.lease);
        let error = resolver
            .worker_for_claimed(&claimed)
            .await
            .err()
            .expect("C3 actual Provider Create must refuse");
        assert_eq!(
            fake.creates.load(Ordering::SeqCst),
            1,
            "C3 Provider Create entered: {error:?}"
        );
        match error {
            awaken_dispatch_execution::Error::ResolutionNotReady(wait) => {
                assert_eq!(wait.retry_after(), None, "C3 original Defer has no hint");
                assert!(wait.to_string().contains("provider quota"));
            }
            other => panic!("C3 wrong claim disposition: {other:?}"),
        }
        assert!(
            store.claim_is_current(&claim).await.unwrap(),
            "C3 exact original owner/epoch"
        );
        let rows = store.list_dispatches().await.unwrap();
        assert_eq!(rows[0].state, DispatchState::Leased);
        assert_eq!(
            rows[0].attempt_count, attempts_before,
            "C3 resolver does not spend budget"
        );
        assert!(!rows[0].sandbox_bound, "C3 no durable Sandbox binding");
        assert_eq!(fake.observations.lock().unwrap().len(), 0);
        assert_eq!(fake.adopts.load(Ordering::SeqCst), 0);
        assert_eq!(fake.disposals.load(Ordering::SeqCst), 0);
        assert!(
            host.realization_executor
                .environments()
                .session_environment("session-container")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            host.realization_executor
                .environments()
                .durable_environment_binding("session-container")
                .unwrap()
                .is_none(),
            "C3 no successful Environment binding"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .read("session-container", |slot| slot.run.context.is_none()
                    && slot.run.dispatch_claim.as_ref() == Some(&claim)
                    && !slot.run.session_dispatch
                    && slot.inputs.baseline.is_none()
                    && slot.realization.execution().authority.lease().is_none()
                    && slot.realization.execution().frozen_receipt.is_none())
                .unwrap()
                .unwrap_or(false),
            "C3 original ordinary inputs/claim remain, without a complete context or Managed authority/receipt"
        );
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
        // or P2 the claim resolver (SessionExecutionError -> map_admission_error).
        // Effects: the claim outcome (with the claim's backoff the dispatch
        // pool returns a `relinquish` through `retry_claim`, spending one
        // dispatch attempt, and a `defer` through `defer_claim`, keeping the
        // budget; `absorb` settles the Run), and the FailSessionRealization
        // report, which decides the Session state (session-application
        // `failure_budget` table):
        // | Rule | class | claim (P1 = P2) | report (retryable, capacity) | Session state | Session budget consumed? | dispatch budget consumed? |
        // | X1 | Transient: NotReady/Transport/Timeout/Backend | relinquish (retry_claim) | (true, false) | Preparing; ActivationFailed once the budget is spent | yes, one attempt | yes, one attempt; Ended(Indeterminate) once spent |
        // | X2 | CapacityUnavailable | defer (request-local) | (true, true) | Preparing, never ActivationFailed | no, attempt refunded | no |
        // | X3 | Permanent: NotFound/Gone/Unauthorized/FenceRejected/Invalid/Unsupported/Conflict | absorb (TerminalResolution) | (false, false) | ActivationFailed | n/a (absorbing) | n/a (Run settled) |
        // Session-state and budget columns are proven on the durable aggregate by
        // session-application `capacity_budget` and the contract's
        // `capacity_failures_retry_without_consuming_and_others_keep_the_budget`;
        // the dispatch column by the pool's `Error::claim_release` table and
        // the dispatch conformance rule
        // `failed_attempts_spend_the_budget_and_exhaust_like_a_crash`.
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
                crate::session_execution_error::from_run_error(
                    awaken_session_realization_contract::environment_effect_error(&sandbox),
                )
            };
            let resolver = awaken_dispatch_execution::session_worker_resolver::map_admission_error(
                host_error(),
            );
            // P1: the driver sees the same Host error after the synchronizer's
            // Run projection, reports it to Control, then returns it.
            let run_error = crate::session_execution_error::to_run_error(host_error());
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
            let reported = (command.retryable, command.not_ready);
            let driver = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(run_error.into()),
            );
            assert_eq!(claim_outcome(&resolver), claim, "{rule} P2 {sandbox}");
            assert_eq!(claim_outcome(&driver), claim, "{rule} P1 {sandbox}");
            assert_eq!(reported, report, "{rule} report {sandbox}");
            assert!(
                resolver.to_string().contains(&sandbox.to_string()),
                "{rule}: the provider detail survives the boundary"
            );
        }
        // An unclassified Host failure relinquishes on the resolver as it does
        // on the driver (a bounded retry), and a capacity code on a
        // non-Unavailable fault is not a capacity refusal.
        assert_eq!(
            claim_outcome(
                &awaken_dispatch_execution::session_worker_resolver::map_admission_error(
                    crate::SessionExecutionError::internal("unclassified")
                )
            ),
            "relinquish"
        );
        assert_eq!(
            claim_outcome(&awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect((
                    awaken_session_types::RunError::internal("unclassified")).into()),
            )),
            "relinquish"
        );
        assert_eq!(
            claim_outcome(&awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect((
                    awaken_session_types::RunError::classified(
                        awaken_session_realization_contract::ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                        "mislabelled",
                    )).into()),
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
                    )
                    .into(),
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
                )
                .into(),
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
    fn a_deferred_adoption_carries_the_providers_wait_to_the_claim() {
        // Decision table: cause = a failed claim adoption; effect = the claim
        // outcome and the retry-after it hands the pool.
        // | A1 | provisioning before its deadline | defer, hint = wait left |
        // | A2 | capacity refusal | defer, no hint (pool backoff) |
        // | A3 | provisioning past its deadline | relinquish |
        use awaken_session_realization::realization_error::RealizationError;
        let run = awaken_agent_contract::agent::run::Id("run-1".into());
        let deadline = |observed| {
            Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                10_000, observed,
            ))
        };
        for (rule, error, outcome, hint) in [
            (
                "A1",
                RealizationError::provisioning(deadline(4_000), "pulling"),
                "defer",
                Some(std::time::Duration::from_millis(6_000)),
            ),
            (
                "A2",
                awaken_provisioning_contract::SandboxError::capacity_unavailable("q").into(),
                "defer",
                None,
            ),
            (
                "A3",
                RealizationError::provisioning(deadline(10_000), "pulling"),
                "relinquish",
                None,
            ),
        ] {
            let claim = crate::host::dispatch_execution::adoption_claim_outcome(&run, &error);
            assert_eq!(claim_outcome(&claim), outcome, "{rule}");
            if let awaken_dispatch_execution::Error::ResolutionNotReady(not_ready) = &claim {
                assert_eq!(not_ready.retry_after(), hint, "{rule}");
            }
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
        // | Repository binding conflict / Run conflict / unclassified internal | relinquish |
        // | provider capacity                       | defer      |
        // | provisioning before the provider's deadline | defer  |
        // | provisioning with no deadline / at or past it | relinquish |
        // | permanent provider / projection conflict / invariant / invalid | absorb |
        let rows: [Row; 16] = [
            (
                "transient provider",
                || SandboxError::timeout("t").into(),
                "relinquish",
            ),
            (
                "provisioning before its deadline",
                || {
                    RealizationError::provisioning(
                        Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                            9, 4,
                        )),
                        "pulling",
                    )
                },
                "defer",
            ),
            (
                "provisioning past its deadline",
                || {
                    RealizationError::provisioning(
                        Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                            9, 9,
                        )),
                        "pulling",
                    )
                },
                "relinquish",
            ),
            (
                "provisioning with no deadline",
                || RealizationError::provisioning(None, "created"),
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
            (
                "Repository binding conflict",
                || {
                    awaken_resource_contract::RepositoryBindingVerifierError::Conflict {
                        detail: "raced".into(),
                    }
                    .into()
                },
                "relinquish",
            ),
            (
                "Run conflict",
                || awaken_session_types::RunError::conflict("raced").into(),
                "relinquish",
            ),
            (
                "unclassified internal",
                || awaken_session_types::RunError::internal("unknown").into(),
                "relinquish",
            ),
        ];
        for (rule, failure, expected) in rows {
            let claim =
                map_admission_error(crate::host::dispatch_execution::admission_error(failure()));
            let synchronizer = awaken_session_types::RunError::from(failure());
            let driver = map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_realization_contract::SessionRealizationEffectFailure::from(
                        failure(),
                    ),
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
            if let awaken_dispatch_execution::Error::ResolutionNotReady(wait) = driver {
                assert_eq!(
                    wait.retry_after(),
                    failure().retry_after(),
                    "{rule} P3 original provider hint"
                );
            }
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
