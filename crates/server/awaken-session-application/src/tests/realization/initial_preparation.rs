use super::*;
use awaken_session_realization_contract::SessionPreparationControl;

struct FixtureSessionPlacement;

impl awaken_session_realization_contract::SessionPreparationPlacement for FixtureSessionPlacement {
    fn requirements(
        &self,
        _session: &PersistedSession,
    ) -> Result<
        awaken_worker_contract::PlacementRequirements,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        Ok(awaken_worker_contract::PlacementRequirements::remote_required())
    }
}

fn externally_placed(id: &str, state: &str, run_free: bool) -> PersistedSession {
    let mut session = persisted(id, false, state);
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        panic!("frozen fixture")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    session.realization_progress.run_free_initial_preparation = run_free;
    session
}

#[tokio::test]
async fn run_free_request_is_one_idempotent_session_root_transition() {
    // Causes: C1 external non-self-hosted placement, C2 initial realization
    // still pending, C3 request already retained, C4 local/self-hosted scope,
    // C5 terminal or active execution.
    // Effects: E1 commit exactly one root revision/intent, E2 exact replay
    // reads without another write, E3 reject unsupported placement before
    // mutation, E4 an already-Idle historical root remains unchanged, E5
    // terminal/active truth denies a new preparation intent.
    // R1=C1+C2+!C3=>E1; R2=C1+C2+C3=>E2;
    // R3=C4=>E3; R4=C1+!C2=>E4; R5=C5=>E5.
    // No rule creates a Run or work table.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        externally_placed("selected", "preparing", false),
    )
    .await;
    create(repo.as_ref(), externally_placed("completed", "idle", false)).await;
    create(
        repo.as_ref(),
        externally_placed("terminated", "activation_failed", false),
    )
    .await;
    let mut active = externally_placed("active", "running", false);
    active.active_activity_epochs.insert(1);
    create(repo.as_ref(), active).await;
    create(
        repo.as_ref(),
        persisted("local-request", false, "preparing"),
    )
    .await;
    create(
        repo.as_ref(),
        persisted("self-hosted-request", true, "preparing"),
    )
    .await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let id = SessionId::from("selected");
    let before = repo.get(&id).await.unwrap();
    let first = application
        .request_run_free_initial_preparation(&id)
        .await
        .expect("R1/E1");
    assert!(
        first.realization_progress.run_free_initial_preparation,
        "R1/E1"
    );
    assert!(first.revision.0 > before.revision.0, "R1 one root CAS");
    let replay = application
        .request_run_free_initial_preparation(&id)
        .await
        .expect("R2/E2");
    assert_eq!(replay.revision, first.revision, "R2/E2 no second CAS");
    for invalid in ["local-request", "self-hosted-request"] {
        let invalid = SessionId::from(invalid);
        assert!(
            matches!(
                application
                    .request_run_free_initial_preparation(&invalid)
                    .await,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                        _
                    )
                )
            ),
            "R3/E3"
        );
        assert!(
            !repo
                .get(&invalid)
                .await
                .unwrap()
                .realization_progress
                .run_free_initial_preparation,
            "R3/E3"
        );
    }
    let completed = SessionId::from("completed");
    let before = repo.get(&completed).await.unwrap();
    let after = application
        .request_run_free_initial_preparation(&completed)
        .await
        .expect("R4/E4");
    assert_eq!(after.revision, before.revision, "R4/E4");
    assert!(
        !after.realization_progress.run_free_initial_preparation,
        "R4/E4"
    );
    for blocked in ["terminated", "active"] {
        let blocked = SessionId::from(blocked);
        assert!(
            matches!(
                application
                    .request_run_free_initial_preparation(&blocked)
                    .await,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::NotReady
                )
            ),
            "R5/E5"
        );
        assert!(
            !repo
                .get(&blocked)
                .await
                .unwrap()
                .realization_progress
                .run_free_initial_preparation,
            "R5/E5"
        );
    }
}

#[tokio::test]
async fn initial_preparation_claims_only_an_eligible_external_preparing_root() {
    // Cause/effect graph: C1 execution is Preparing, C2 placement is external
    // but not self-hosted, C3 Registry incarnation is live Ready, C4 manifest
    // satisfies frozen Sandbox/Resources, C5 a foreign live realization lease
    // is absent; C6 the canonical placement compiler is installed; C7 the
    // complete root explicitly requests Run-free preparation. E1 is one
    // root-CAS assignment with the exact frozen Session;
    // E2 is no assignment, no root revision change, and no model Run.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Effect |
    // | R1 | yes | yes | yes | yes | yes | yes | yes | E1 |
    // | R2 | no | any | yes | yes | yes | yes | yes | E2 |
    // | R3 | yes | no | yes | yes | yes | yes | yes | E2 |
    // | R4 | yes | yes | no | yes | yes | yes | yes | E2 |
    // | R5 | yes | yes | yes | no | yes | yes | yes | E2 |
    // | R6 | yes | yes | yes | yes | no | yes | yes | E2 |
    // | R7 | yes | yes | yes | yes | yes | no | yes | unavailable |
    // | R9 | yes | yes | yes | yes | yes | yes | no | E2 |
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut local = persisted("local", false, "preparing");
    local.realization_progress.run_free_initial_preparation = true;
    create(repo.as_ref(), local).await;
    let mut self_hosted = persisted("self-hosted", true, "preparing");
    self_hosted
        .realization_progress
        .run_free_initial_preparation = true;
    create(repo.as_ref(), self_hosted).await;
    create(repo.as_ref(), externally_placed("idle", "idle", true)).await;
    create(
        repo.as_ref(),
        externally_placed("normal", "preparing", false),
    )
    .await;
    create(
        repo.as_ref(),
        externally_placed("target", "preparing", true),
    )
    .await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let observations = Arc::new(RecordingWorkerObservations::default());
    let eligible = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    observations.replace(vec![eligible.clone()]);
    application
        .set_worker_observation_source(observations.clone())
        .unwrap();
    assert!(
        matches!(
            application
                .claim_next_session_preparation(target.clone())
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "R7 missing canonical compiler fails closed"
    );
    application
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();

    let mut incapable = eligible.clone();
    incapable.snapshot.manifest.capacity.max_concurrent = 0;
    incapable.snapshot.capability_fingerprint = incapable.snapshot.manifest.fingerprint().unwrap();
    observations.replace(vec![incapable]);
    assert!(
        application
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "R5/E2"
    );

    let mut draining = eligible.clone();
    draining.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    observations.replace(vec![draining]);
    assert!(
        application
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "R4/E2"
    );

    observations.replace(vec![eligible]);
    let assignment = application
        .claim_next_session_preparation(target.clone())
        .await
        .unwrap()
        .expect("R1/E1 eligible root");
    assert_eq!(assignment.session_id.as_str(), "target", "R1/E1");
    assert_eq!(
        assignment.directive.authority.lease.owner, "worker",
        "R1/E1"
    );
    assert!(
        repo.get(&SessionId::from("local"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "R3/E2"
    );
    assert!(
        repo.get(&SessionId::from("self-hosted"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "R3/E2"
    );
    assert!(
        repo.get(&SessionId::from("idle"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "R2/E2"
    );
    assert!(
        repo.get(&SessionId::from("normal"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "R9/E2"
    );

    let another = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "other".into(),
        runtime_incarnation: "other:1:boot".into(),
        ..target
    };
    observations.replace(vec![terminal_cleanup_worker(
        &another,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .claim_next_session_preparation(another)
            .await
            .unwrap()
            .is_none(),
        "R6/E2"
    );
}

#[tokio::test]
async fn preparation_rechecks_worker_capability_after_root_assignment() {
    // R8 adds C7, a Registry capability withdrawal between the first
    // eligibility read and the root CAS readback. E3 is no directive delivered
    // to a physical Worker; the assigned lease remains root-owned and expires
    // through its ordinary fence. R8=C1..C6 true + C7 withdrawn => E3.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), externally_placed("race", "preparing", true)).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let eligible = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    let mut withdrawn = eligible.clone();
    withdrawn.snapshot.manifest.capacity.max_concurrent = 0;
    withdrawn.snapshot.capability_fingerprint = withdrawn.snapshot.manifest.fingerprint().unwrap();
    let observations = Arc::new(RecordingWorkerObservations::default());
    observations.script([
        WorkerObservationStep::Workers(vec![eligible]),
        WorkerObservationStep::Workers(vec![withdrawn]),
    ]);
    application
        .set_worker_observation_source(observations)
        .unwrap();
    application
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();
    assert!(
        application
            .claim_next_session_preparation(target)
            .await
            .unwrap()
            .is_none(),
        "R8/E3"
    );
    let root = repo.get(&SessionId::from("race")).await.unwrap();
    assert_eq!(root.execution, SessionExecutionState::Preparing, "R8/E3");
    assert!(
        root.realization.is_some(),
        "R8 ordinary lease remains durable"
    );
}
