use super::*;
use awaken_session_execution_contract::{
    AcknowledgeSessionRealization, BeginSessionRealization, EnvironmentRuntime,
    SessionContinuationReceipt as Receipt, SessionPreparationControl,
    SessionRealizationAction as Action, SessionRealizationControl, SessionRealizationTarget,
};

#[tokio::test]
async fn registered_cloud_checkpoint_creation_accepts_only_the_durable_root() {
    // C1 RegisteredWorker topology; C2 cloud (not self-hosted WorkQueue);
    // C3 valid checkpoint retention; C4 accept-durable-root completion;
    // C5 the profiled Managed adapter's explicit initial-preparation request.
    // C1..C4 => E1 exact frozen policy/Worker placement survives root insert;
    // E2 Preparing root is recoverable; C5=false keeps the existing opt-in
    // boundary, C5=true advertises run-free recovery without a fabricated
    // readiness receipt. E3 no Coordinator preparation, projection or WorkQueue
    // dispatch. The self-hosted C2=false refusal is owned by creation's existing
    // worker_checkpoint_retention_is_rejected_before_durable_creation test.
    let repo: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(super::super::creation::RecordingCreateRuntime::default());
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
    );
    let mut command = super::super::creation::creation_command("registered-cloud-checkpoint");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    command.intent.control.environment.self_hosted = false;
    let retention = checkpoint_release_retention();
    command.intent.control.environment.idle_retention = retention.clone();
    let created = application
        .creation_commands
        .accept_session(command)
        .await
        .expect("E1 supported cloud Worker checkpoint policy");
    assert_eq!(repo.get(&created.session_id).await.unwrap(), created, "E1");
    let baseline = created.frozen_baseline().unwrap();
    assert_eq!(baseline.environment.idle_retention, retention, "E1");
    assert_eq!(
        baseline.runtime_placement,
        awaken_session_types::SessionRuntimePlacement::Worker,
        "E1"
    );
    assert_eq!(created.execution, SessionExecutionState::Preparing, "E2");
    assert!(created.needs_initial_realization_recovery(), "E2");
    assert!(!created.is_run_free_preparation_candidate(), "E2 opt-in");
    assert!(created.realization.is_none(), "E2 no invented Worker grant");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(environments.dispatch_calls.load(Ordering::SeqCst), 0, "E3");
    application
        .realizations
        .request_run_free_initial_preparation(&created.session_id)
        .await
        .expect("E2 profiled Managed adapter uses the existing explicit opt-in");
    let requested = repo.get(&created.session_id).await.unwrap();
    assert!(
        requested.is_run_free_preparation_candidate(),
        "E2 requested"
    );
    assert_eq!(requested.frozen_baseline(), created.frozen_baseline(), "E1");
    assert!(
        requested.realization.is_none(),
        "E2 no invented Worker grant"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(environments.dispatch_calls.load(Ordering::SeqCst), 0, "E3");
}

#[tokio::test]
async fn worker_continuation_reloads_root_receipts_and_releases_then_restores_one_session() {
    // Causes: C1 frozen registered Worker placement; C2 exact live source
    // lease; C3 Worker continuation capability; C4 typed matching receipt;
    // C5 lost response / fresh application; C6 target not yet admitted.
    // Effects: E1 only root-authored next effect; E2 reject with no root CAS;
    // E3 Hibernated has no retained Worker; E4 Restore gets a new lease and
    // write-ahead pending before provider I/O; E5 receipt lands pending->origin.
    // W1 !C3 or !C2 => E2. W2 C1..C4 => four suspend phases in order.
    // W3 C5 => reload committed next phase (no second queue or receipt store).
    // W4 source disposed => E3; driving ingress+C6 => unbound demand.
    // W5 restore claim => E4; exact restore receipt => E5 + frozen reprojection.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = SessionId::from("worker-continuation");
    let mut source = externally_placed(generated_session(id.as_str()));
    let target = SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    source.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: target.runtime_incarnation.clone(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    });
    create(repo.as_ref(), source).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let workers = Arc::new(RecordingWorkerObservations::default());
    let app = || {
        configured_application(
            runtime.clone(),
            crate::SessionPersistence::new(
                repo.clone(),
                Arc::new(awaken_lease_testkit::ManualClock::new(2_000)),
            ),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
            {
                let probe = Arc::new(crate::ExecutionObservationProbe::default());
                crate::SessionExecutionAdmission::new(probe.clone(), workers.clone(), probe)
            },
        )
    };
    let application = app();
    application
        .continuations
        .reconcile_environment_continuation(&id, 2_000)
        .await
        .unwrap();
    let suspending = repo.get(&id).await.unwrap();
    assert!(suspending.is_run_free_preparation_candidate());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .realizations
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "W1 missing capability"
    );
    assert_eq!(repo.get(&id).await.unwrap(), suspending, "W1 no root CAS");
    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_ENVIRONMENT_CONTINUATION_CAPABILITY.into());
    // W1 source capture refuses a replacement incarnation and a forced
    // same-incarnation epoch change before CAS, even with the capability.
    let replacement = SessionRealizationTarget {
        runtime_incarnation: "worker:2:replacement".into(),
        ..target.clone()
    };
    workers.replace(vec![terminal_cleanup_worker(
        &replacement,
        manifest.clone(),
    )]);
    assert!(
        application
            .realizations
            .begin_session_realization(BeginSessionRealization {
                session_id: id.clone(),
                target: replacement,
                expected_resources: None
            })
            .await
            .is_err(),
        "W1 replacement cannot strand original source"
    );
    assert_eq!(
        repo.get(&id).await.unwrap(),
        suspending,
        "W1 replacement zero CAS"
    );
    workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
    let forced = SessionRealizationTarget {
        reassign_existing_lease: true,
        ..target.clone()
    };
    assert!(
        application
            .realizations
            .begin_session_realization(BeginSessionRealization {
                session_id: id.clone(),
                target: forced,
                expected_resources: None
            })
            .await
            .is_err(),
        "W1 forced epoch cannot strand source"
    );
    assert_eq!(
        repo.get(&id).await.unwrap(),
        suspending,
        "W1 forced epoch zero CAS"
    );
    let mut directive = application
        .realizations
        .claim_next_session_preparation(target.clone())
        .await
        .unwrap()
        .expect("W2 exact source owner")
        .directive;
    // W9 delayed ordinary Stage/Publish callbacks cannot cross the closed
    // continuation phase, even when their generation sets happen to be empty.
    assert!(
        application
            .realizations
            .activate_session_realization(
                awaken_session_execution_contract::ActivateSessionRealization {
                    session_id: id.clone(),
                    lease: directive.authority.lease.clone(),
                    prepared_resource_revision: None,
                    mcp_receipts: vec![],
                }
            )
            .await
            .is_err(),
        "W9 ordinary activation refused"
    );
    assert!(
        application
            .realizations
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: id.clone(),
                lease: directive.authority.lease.clone(),
                published: vec![],
                drained: vec![],
                continuation: None,
            })
            .await
            .is_err(),
        "W9 ordinary acknowledgement refused"
    );
    assert_eq!(
        repo.get(&id).await.unwrap(),
        suspending,
        "W9 no root mutation"
    );
    // W7 effect failure must not use ordinary Stage failure/lease expiry:
    // the unchanged root can retry its still-current source generation.
    let projection = DriverProjection {
        fail_install: AtomicBool::new(false),
        installed: AtomicUsize::new(0),
        invalidated: AtomicUsize::new(0),
    };
    for fail_install in [false, true] {
        projection
            .fail_install
            .store(fail_install, Ordering::SeqCst);
        assert!(
            crate::realization::drive_test_directive(
                application.realizations.as_ref(),
                &id,
                &projection,
                directive.clone(),
                &NoopMcpRealizer
            )
            .await
            .is_err(),
            "W7 injected failure"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            suspending,
            "W7 no failure transition or expiry"
        );
        assert_eq!(
            projection.invalidated.load(Ordering::SeqCst),
            0,
            "W7 grant retained"
        );
    }
    projection.fail_install.store(false, Ordering::SeqCst);
    let worker_runtime = ContinuationRuntime::default();
    for expected in [
        awaken_session_contract::SessionEnvironmentInstruction::Quiesce,
        awaken_session_contract::SessionEnvironmentInstruction::Checkpoint,
        awaken_session_contract::SessionEnvironmentInstruction::PrepareSourceRelease,
        awaken_session_contract::SessionEnvironmentInstruction::DisposeSource,
    ] {
        assert_eq!(
            directive.action,
            Action::Continue {
                instruction: expected
            },
            "W2 exact next effect"
        );
        let root = repo.get(&id).await.unwrap();
        let receipt = if expected == awaken_session_contract::SessionEnvironmentInstruction::Quiesce
        {
            let (operation, effect, binding, generation) =
                root.environment.suspend_source().unwrap();
            Receipt::Quiesced(
                worker_runtime
                    .quiesce_session_environment(
                        id.as_str(),
                        operation,
                        effect,
                        binding,
                        generation,
                        &[],
                    )
                    .await
                    .unwrap(),
            )
        } else {
            awaken_session_execution_contract::execute_environment_provider_continuation(
                "workspace",
                &id,
                &root.environment,
                &root.frozen_baseline().unwrap().environment.idle_retention,
                Some(&directive.authority.lease),
                expected,
                &worker_runtime,
            )
            .await
            .unwrap()
        };
        let mut stale = directive.authority.lease.clone();
        stale.runtime_incarnation = "foreign:1:boot".into();
        assert!(
            application
                .realizations
                .acknowledge_session_realization(AcknowledgeSessionRealization {
                    session_id: id.clone(),
                    lease: stale,
                    published: vec![],
                    drained: vec![],
                    continuation: Some(receipt.clone()),
                })
                .await
                .is_err(),
            "W1 receipt from foreign delivery"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            root,
            "W1 receipt refusal has no mutation"
        );
        directive = application
            .realizations
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: id.clone(),
                lease: directive.authority.lease.clone(),
                published: vec![],
                drained: vec![],
                continuation: Some(receipt),
            })
            .await
            .unwrap();
        if expected == awaken_session_contract::SessionEnvironmentInstruction::Checkpoint {
            // Drop the successful response; a fresh Control reconstructs it.
            directive = app()
                .realizations
                .begin_session_realization(BeginSessionRealization {
                    session_id: id.clone(),
                    target: target.clone(),
                    expected_resources: None,
                })
                .await
                .unwrap();
            assert!(matches!(
                directive.action,
                Action::Continue {
                    instruction:
                        awaken_session_contract::SessionEnvironmentInstruction::PrepareSourceRelease
                }
            ));
        }
    }
    assert_eq!(directive.action, Action::Released, "W4 zero-I/O release");
    // W8 Released is processed before any projection install or provider work.
    let before_installs = projection.installed.load(Ordering::SeqCst);
    crate::realization::drive_test_directive(
        application.realizations.as_ref(),
        &id,
        &projection,
        directive.clone(),
        &NoopMcpRealizer,
    )
    .await
    .unwrap();
    assert_eq!(
        projection.installed.load(Ordering::SeqCst),
        before_installs,
        "W8 zero install"
    );
    assert_eq!(
        projection.invalidated.load(Ordering::SeqCst),
        1,
        "W8 exact predecessor invalidated"
    );
    let hibernated = repo.get(&id).await.unwrap();
    assert!(
        !hibernated.has_unfinished_worker_realization().unwrap(),
        "W4 no retained capacity"
    );
    assert!(
        !hibernated.needs_run_free_preparation_worker(),
        "W4 no active capacity demand"
    );
    let restoring = application
        .continuations
        .ensure_environment_resident(&id, 2_000)
        .await
        .unwrap();
    assert!(
        restoring.needs_run_free_preparation_worker(),
        "W4 old lease is historical"
    );
    // W6 filesystem absence permits zero provider I/O, but does not prove
    // process/dispatch closure. Terminal demand retains the original actor;
    // after expiry a different actor needs exact predecessor-exit evidence.
    for mut terminal in [hibernated.clone(), restoring.clone()] {
        terminal.archive("2026-11-10T00:00:00Z", 2_000).unwrap();
        assert!(
            terminal.has_unfinished_worker_realization().unwrap(),
            "W6 terminal actor still has cleanup responsibility"
        );
        assert!(
            matches!(
                terminal.worker_obligation().unwrap(),
                awaken_session_contract::SessionWorkerObligation::RetainedOwner(_)
            ),
            "W6 actor retention differs from live Sandbox capacity"
        );
        assert_eq!(
            awaken_session_contract::session_terminal_cleanup_affinity_for(
                &terminal,
                "new-worker",
                "new-worker:1:boot",
                2_000
            ),
            awaken_session_contract::SessionTerminalCleanupAffinity::Retained,
            "W6 no unproved live handoff"
        );
        terminal.realization.as_mut().unwrap().expires_at_unix_ms = 1;
        assert_eq!(
            awaken_session_contract::session_terminal_cleanup_affinity_for(
                &terminal,
                "new-worker",
                "new-worker:1:boot",
                2_000
            ),
            awaken_session_contract::SessionTerminalCleanupAffinity::RequiresPredecessorExit,
            "W6 existing exact exit port remains required"
        );
    }
    let restore = app()
        .realizations
        .claim_next_session_preparation(target)
        .await
        .unwrap()
        .expect("W5 restore demand")
        .directive;
    assert_eq!(
        restore.action,
        Action::Continue {
            instruction: awaken_session_contract::SessionEnvironmentInstruction::Restore
        }
    );
    assert!(
        restore.authority.lease.epoch > directive.authority.lease.epoch,
        "W5 exact successor kernel"
    );
    let pending = repo.get(&id).await.unwrap();
    assert!(
        pending
            .environment_materialization()
            .unwrap()
            .pending()
            .is_some(),
        "W5 before provider I/O"
    );
    let receipt = awaken_session_execution_contract::execute_environment_provider_continuation(
        "workspace",
        &id,
        &pending.environment,
        &pending
            .frozen_baseline()
            .unwrap()
            .environment
            .idle_retention,
        Some(&restore.authority.lease),
        awaken_session_contract::SessionEnvironmentInstruction::Restore,
        &worker_runtime,
    )
    .await
    .unwrap();
    let completed = application
        .realizations
        .acknowledge_session_realization(AcknowledgeSessionRealization {
            session_id: id.clone(),
            lease: restore.authority.lease,
            published: vec![],
            drained: vec![],
            continuation: Some(receipt),
        })
        .await
        .unwrap();
    assert!(
        matches!(
            completed.action,
            Action::Stage {
                prepare_session: true,
                ..
            }
        ),
        "W5 complete frozen projection"
    );
    let resident = repo.get(&id).await.unwrap();
    assert_eq!(resident.environment.binding(), Some("restored-binding"));
    let materialization = resident.environment_materialization().unwrap();
    assert!(materialization.pending().is_none());
    assert_eq!(
        materialization.origin(),
        pending.environment_materialization().unwrap().pending(),
        "W5 first target provenance"
    );
    assert_eq!(
        runtime.quiesces.load(Ordering::SeqCst)
            + runtime.checkpoints.load(Ordering::SeqCst)
            + runtime.restores.load(Ordering::SeqCst),
        0,
        "all Coordinator physical I/O forbidden"
    );
}

struct DriverProjection {
    fail_install: AtomicBool,
    installed: AtomicUsize,
    invalidated: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionProjectionSynchronizer for DriverProjection {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_execution_contract::SessionRealizationEffectFailure,
    > {
        test_realization_request_started().map_err(Into::into)
    }
    async fn synchronize_session_projection(
        &self,
        _: &SessionId,
        _: &awaken_session_execution_contract::FrozenSessionProjection,
        _: &awaken_session_execution_contract::ObservedSessionRealizationLease,
        _: bool,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure> {
        self.installed.fetch_add(1, Ordering::SeqCst);
        if self.fail_install.load(Ordering::SeqCst) {
            return Err(RunError::unavailable("injected projection install failure").into());
        }
        Ok(())
    }
    async fn execute_environment_continuation(
        &self,
        _: &awaken_session_execution_contract::SessionRealizationPermit,
        _: &awaken_session_execution_contract::FrozenSessionProjection,
        _: &crate::session_resource::SessionRealizationLease,
        _: awaken_session_contract::SessionEnvironmentInstruction,
    ) -> Result<Receipt, awaken_session_execution_contract::SessionRealizationEffectFailure> {
        Err(RunError::unavailable("injected continuation transport failure").into())
    }
    async fn invalidate_session_realization_lease(
        &self,
        _: &SessionId,
        _: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure> {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn worker_expiry_deletes_exact_reference_then_admits_a_fresh_generation() {
    // E1 unexpired Hibernated is only a scan hint: no claim or capacity.
    // E2 expired + compatible Worker => exact idempotent delete, never restore.
    // E3 mismatching deletion receipt => no CAS. E4 exact receipt => release,
    // retire current origin, preserve historical non-no-Create marker.
    // E5 next materialization obtains a new fence and may admit one Create.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = SessionId::from("worker-expired");
    let source = externally_placed(hibernated_session(id.as_str(), 1_000));
    create(repo.as_ref(), source).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let workers = Arc::new(RecordingWorkerObservations::default());
    let target = SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_ENVIRONMENT_CONTINUATION_CAPABILITY.into());
    workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
    let probe = Arc::new(crate::ExecutionObservationProbe::default());
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(
            repo.clone(),
            Arc::new(awaken_lease_testkit::ManualClock::new(2_000)),
        ),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
        crate::SessionExecutionAdmission::new(probe.clone(), workers, probe),
    );
    let directive = app
        .realizations
        .claim_next_session_preparation(target.clone())
        .await
        .unwrap()
        .expect("E2 expired reference")
        .directive;
    assert_eq!(
        directive.action,
        Action::Continue {
            instruction:
                awaken_session_contract::SessionEnvironmentInstruction::DeleteExpiredCheckpoint
        }
    );
    let before = repo.get(&id).await.unwrap();
    let command = |checkpoint_id: &str| AcknowledgeSessionRealization {
        session_id: id.clone(),
        lease: directive.authority.lease.clone(),
        published: vec![],
        drained: vec![],
        continuation: Some(Receipt::ExpiredCheckpointDeleted {
            checkpoint_id: checkpoint_id.into(),
        }),
    };
    assert!(
        app.realizations
            .acknowledge_session_realization(command("foreign"))
            .await
            .is_err(),
        "E3"
    );
    assert_eq!(repo.get(&id).await.unwrap(), before, "E3 zero CAS");
    let released = app
        .realizations
        .acknowledge_session_realization(command("object"))
        .await
        .unwrap();
    assert_eq!(released.action, Action::Released, "E4");
    let empty = repo.get(&id).await.unwrap();
    assert!(matches!(
        empty.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
    assert!(
        !awaken_session_contract::session_has_no_create_evidence(&empty),
        "E4 historical absence is not fresh no-Create"
    );
    let next = app
        .realizations
        .begin_session_realization(BeginSessionRealization {
            session_id: id.clone(),
            target,
            expected_resources: None,
        })
        .await
        .unwrap();
    assert!(
        next.authority.lease.epoch > directive.authority.lease.epoch,
        "E5 revoked predecessor never reused"
    );
    let mut aggregate =
        awaken_session_contract::SessionAggregate::try_from(repo.get(&id).await.unwrap()).unwrap();
    let intent = awaken_session_contract::SessionEnvironmentEffectIntent::new(
        id,
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        Some(next.authority.lease),
    )
    .for_environment(
        aggregate
            .frozen_baseline()
            .unwrap()
            .environment
            .config_fingerprint
            .0
            .clone(),
    );
    aggregate
        .environment()
        .admit_environment_effect("workspace", &intent, 2_000)
        .expect("E5 new current generation admits Create");
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0, "E2 no restore");
}

#[tokio::test]
async fn local_restore_assigns_and_installs_a_fresh_target_after_hibernated_lease_expiry() {
    // L1 a tracked restore follows Hibernated whose old lease has expired.
    // L2 the local target uses the canonical assignment and pending intent CAS.
    // L3 the Runtime refuses Restore unless its exact new grant/projection was
    // installed first. E1 one restore, Resident with new origin; E2 historical
    // operation coordinates remain immutable; E3 original expired epoch is not
    // reused. Legacy unknown target refusal remains in the adjacent O3 table.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = SessionId::from("local-expired-restore-grant");
    let mut root = hibernated_session(id.as_str(), u64::MAX);
    let old_epoch = root.realization.as_ref().unwrap().epoch;
    root.realization.as_mut().unwrap().expires_at_unix_ms = 1;
    create(repo.as_ref(), root).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime
        .require_restore_projection
        .store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let resident = app
        .continuations
        .ensure_environment_resident(&id, 1_000)
        .await
        .unwrap();
    assert!(matches!(
        resident.environment,
        awaken_session_contract::SessionEnvironmentState::Resident { .. }
    ));
    let granted = resident.realization.as_ref().unwrap();
    assert!(
        granted.epoch > old_epoch,
        "L3 successor from existing epoch kernel"
    );
    assert_eq!(
        granted.owner,
        app.realizations.local_realization_owner(),
        "L2 actual local owner"
    );
    assert_eq!(
        resident
            .environment_materialization()
            .unwrap()
            .origin()
            .unwrap()
            .realization(),
        Some(granted),
        "L2 first target provenance"
    );
    assert_eq!(
        runtime.restores.load(Ordering::SeqCst),
        1,
        "L1/L3 exactly one restore"
    );
}
