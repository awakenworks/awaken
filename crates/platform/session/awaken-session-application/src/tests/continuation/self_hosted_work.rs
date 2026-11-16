use super::*;

/// The source Worker of every fixture root: one registered incarnation that
/// advertises the continuation protocol.
fn source_lease() -> crate::session_resource::SessionRealizationLease {
    crate::session_resource::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: awaken_worker_contract::WorkerIdentity::new("worker-a", "boot-1", 1)
            .lease_owner(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX,
    }
}

fn worker_placed(id: &str, self_hosted: bool) -> PersistedSession {
    let mut session = externally_placed(generated_session(id));
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        unreachable!("continuation fixture baseline is frozen")
    };
    baseline.environment.self_hosted = self_hosted;
    session.realization = Some(source_lease());
    session
}

#[tokio::test]
async fn a_self_hosted_worker_suspend_wakes_its_session_work_until_it_hibernates() {
    // Causes: C1 Worker placement with a self-hosted Environment (its Worker
    // effects are fenced by Session Work, stopped since the last Run
    // settled); C2 the frozen checkpoint-and-release deadline passed on an
    // idle generated Resident root; C3 a driving activity is open; C4 the
    // root is Hibernated.
    // Effects: E1 the Coordinator commits the suspend intent; E2 it wakes the
    // Session Work so the source Worker can claim the continuation; E3 the
    // root is a Run-free continuation candidate; E4 nothing is woken.
    // | Rule | C1 | C2 | C3 | C4 | Effects |
    // | S1 | yes | yes | no | no | E1 + E2 + E3 |
    // | S2 | no (cloud Worker) | yes | no | no | E1 + E3, no wake |
    // | S3 | yes | (Suspending) | yes | no | E4: the Run's admission woke it |
    // | S4 | yes | - | no | yes | E4 (live checkpoint: no effect is decided) |
    // | S5 | yes | yes | no | no | E1 + E2 despite a racing root commit |
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let workers = Arc::new(RecordingWorkerObservations::default());
    let lease = source_lease();
    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_ENVIRONMENT_CONTINUATION_CAPABILITY.into());
    workers.replace(vec![terminal_cleanup_worker(
        &awaken_session_execution_contract::SessionRealizationTarget {
            owner: lease.owner.clone(),
            runtime_incarnation: lease.runtime_incarnation.clone(),
            lease_expires_at_unix_ms: u64::MAX,
            reassign_existing_lease: false,
        },
        manifest,
    )]);
    let application = configured_application(
        Arc::new(ContinuationRuntime::default()),
        crate::SessionPersistence::new(
            repository.clone(),
            Arc::new(awaken_lease_testkit::ManualClock::new(2_000)),
        ),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), workers.clone(), probe)
        },
    );
    let awakened = |id: &str| environments.awakened.lock().unwrap().contains(id);
    let suspending = |root: &PersistedSession| {
        matches!(
            root.environment,
            awaken_session_contract::SessionEnvironmentState::Suspending { .. }
        )
    };

    for (rule, id, self_hosted) in [("S1", "self-hosted", true), ("S2", "cloud", false)] {
        create(repository.as_ref(), worker_placed(id, self_hosted)).await;
        let session_id = SessionId::from(id);
        application
            .continuations
            .reconcile_environment_continuation(&session_id, 2_000)
            .await
            .unwrap_or_else(|error| panic!("{rule} {error}"));
        let root = repository.get(&session_id).await.unwrap();
        assert!(suspending(&root), "{rule}/E1 {:?}", root.environment);
        assert_eq!(awakened(id), self_hosted, "{rule}/E2");
        assert_eq!(
            awaken_session_contract::continuation_wakes_session_work(&root),
            self_hosted,
            "{rule}/E2"
        );
        assert!(root.is_run_free_preparation_candidate(), "{rule}/E3");
        // A later pass over the same unfinished suspend repeats the
        // idempotent wake and never moves the committed phase.
        application
            .continuations
            .reconcile_environment_continuation(&session_id, 2_000)
            .await
            .unwrap_or_else(|error| panic!("{rule} repeat {error}"));
        assert_eq!(repository.get(&session_id).await.unwrap(), root, "{rule}");
    }

    // S3: a Run admitted mid-suspend opened an activity; its admission woke
    // the Work, so the continuation wakes nothing more.
    let mut driven = worker_placed("driven", true);
    let mut aggregate =
        awaken_session_contract::SessionAggregate::try_from(driven.clone()).unwrap();
    aggregate
        .environment()
        .begin_suspend_at("workspace", 2_000)
        .unwrap();
    driven = aggregate.into_snapshot();
    // The admitted Run's activity epoch, above every epoch the fixture used.
    let admitted_epoch = 9;
    driven.activity_epoch = admitted_epoch;
    driven.active_activity_epochs.insert(admitted_epoch);
    driven.execution = awaken_session_contract::SessionExecutionState::Running;
    create(repository.as_ref(), driven.clone()).await;
    application
        .continuations
        .reconcile_environment_continuation(&SessionId::from("driven"), 2_000)
        .await
        .unwrap();
    assert!(!awakened("driven"), "S3/E4");
    assert!(!driven.is_run_free_preparation_candidate(), "S3");

    // S4: a Hibernated root holds no Work; its restore rides the next Run.
    let mut hibernated = worker_placed("hibernated", true);
    let generation = hibernated.environment.generation().cloned().unwrap();
    hibernated.environment = awaken_session_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_provisioning_contract::SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "awaken-fs-tar-v1".into(),
            digest: "digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: generation.environment_fingerprint().to_owned(),
            base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "suspend".into(),
        },
        generation,
    };
    create(repository.as_ref(), hibernated.clone()).await;
    application
        .continuations
        .reconcile_environment_continuation(&SessionId::from("hibernated"), 2_000)
        .await
        .unwrap();
    assert!(!awakened("hibernated"), "S4/E4");
    assert!(
        hibernated.is_run_free_preparation_candidate(),
        "S4 the expiry hint stays indexed; the selection skips it while its Work is stopped"
    );

    // S5: a concurrent root commit races the suspend intent (here the intent
    // applies but its response reports a conflict). Nothing physical ran, so
    // the same pass re-decides from the fresh root: the root is Suspending
    // and its Work is woken without waiting for the next recovery cycle.
    let durable: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let faulting = Arc::new(super::FaultingSessionRepository::new(durable.clone()));
    create(durable.as_ref(), worker_placed("raced", true)).await;
    faulting.commit_then_conflict_once("environment-suspend-intent");
    let raced_environments = Arc::new(RecordingEnvironmentSource::default());
    let raced = configured_application(
        Arc::new(ContinuationRuntime::default()),
        crate::SessionPersistence::new(
            faulting,
            Arc::new(awaken_lease_testkit::ManualClock::new(2_000)),
        ),
        raced_environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), workers.clone(), probe)
        },
    );
    raced
        .continuations
        .reconcile_environment_continuation(&SessionId::from("raced"), 2_000)
        .await
        .unwrap_or_else(|error| panic!("S5 {error}"));
    assert!(
        suspending(&durable.get(&SessionId::from("raced")).await.unwrap()),
        "S5 committed once"
    );
    assert!(
        raced_environments
            .awakened
            .lock()
            .unwrap()
            .contains("raced"),
        "S5 woken in the same pass"
    );
}
