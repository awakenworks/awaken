use super::*;
use awaken_session_realization_contract::SessionPreparationControl;

struct FixtureSessionPlacement;

struct SelectiveSessionPlacement;

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

impl awaken_session_realization_contract::SessionPreparationPlacement
    for SelectiveSessionPlacement
{
    fn requirements(
        &self,
        session: &PersistedSession,
    ) -> Result<
        awaken_worker_contract::PlacementRequirements,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let mut requirements = awaken_worker_contract::PlacementRequirements::remote_required();
        if session.session_id.as_str().starts_with("gpu-") {
            requirements.required_capabilities.insert("gpu".into());
        }
        Ok(requirements)
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
async fn run_free_preparation_is_worker_demand_before_any_run_or_owner() {
    // Causes: C1 an external Preparing root explicitly requested Run-free
    // preparation; C2 no realization owner or Dispatch exists; C3 the same
    // root has not requested preparation or has already reached Idle.
    // Effects: R1=C1+C2 contributes one unbound Worker demand, enough for a
    // zero-Worker fleet to wake; R2=C3 contributes zero. This is only a
    // capacity hint: the existing root-CAS claim still owns admission.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        externally_placed("pending", "preparing", true),
    )
    .await;
    create(
        repo.as_ref(),
        externally_placed("ordinary", "preparing", false),
    )
    .await;
    create(repo.as_ref(), externally_placed("finished", "idle", true)).await;
    assert_eq!(
        repo.worker_demand().await.unwrap(),
        1,
        "R1 one root-owned Run-free demand",
    );
    assert!(
        repo.get(&SessionId::from("pending"))
            .await
            .unwrap()
            .needs_run_free_initial_preparation_worker(),
        "R1 retained root classified before Worker registration",
    );
    assert!(
        !repo
            .get(&SessionId::from("ordinary"))
            .await
            .unwrap()
            .needs_run_free_initial_preparation_worker(),
        "R2 ordinary Preparing root has no Run-free capacity request",
    );
    assert!(
        !repo
            .get(&SessionId::from("finished"))
            .await
            .unwrap()
            .needs_run_free_initial_preparation_worker(),
        "R2 completed preparation has no capacity request",
    );
}

#[tokio::test]
async fn preparation_candidate_index_skips_a_saturated_live_owner_front() {
    // Causes: C1 300 otherwise idle roots retain distinct physical Worker
    // owners but have no preparation intent; C2 one later external Preparing
    // root requests Run-free preparation; C3 one Ready compatible Worker asks
    // for the next assignment. Effects: R1=C1+C2 -> the candidate page contains
    // only C2, not the 256-root generic recovery front; R2=R1+C3 -> exact
    // root-CAS assignment of C2 without a model Run. Existing admission tests
    // own capability, incarnation, and competing-claim rejection.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    for index in 0..300 {
        let mut root = externally_placed(&format!("retained-{index}"), "idle", false);
        root.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
            owner: format!("old-worker-{index}"),
            runtime_incarnation: format!("old-worker-{index}:pod"),
            epoch: 1,
            expires_at_unix_ms: 1,
        });
        create(repo.as_ref(), root).await;
    }
    create(
        repo.as_ref(),
        externally_placed("indexed-preparation", "preparing", true),
    )
    .await;
    let page = repo.preparation_candidates(None).await.unwrap();
    assert_eq!(page.sessions.len(), 1, "R1 one indexed candidate");
    assert_eq!(
        page.sessions[0].session.session_id.as_str(),
        "indexed-preparation",
        "R1 generic live-owner front is excluded"
    );
    assert!(page.next_cursor.is_none(), "R1 no hidden candidate page");
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let observations = Arc::new(RecordingWorkerObservations::default());
    observations.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(observations)
        .unwrap();
    application
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();
    let assignment = application
        .claim_next_session_preparation(target)
        .await
        .unwrap()
        .expect("R2 indexed preparation assignment");
    assert_eq!(assignment.session_id.as_str(), "indexed-preparation", "R2");
}

#[tokio::test]
async fn bound_incomplete_preparation_remains_a_candidate_without_extra_replica_demand() {
    // C1 an explicit preparation has a current physical owner but remains in
    // Preparing; C2 a later exact retry may need that same owner. R1=C1+C2 ->
    // the candidate index retains the root, while capacity counts only its
    // distinct retained owner and adds no second first-placement replica.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut root = externally_placed("bound-preparation", "preparing", true);
    root.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:pod".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), root).await;
    let page = repo.preparation_candidates(None).await.unwrap();
    assert_eq!(page.sessions.len(), 1, "R1 same-owner retry candidate");
    assert_eq!(
        page.sessions[0].session.session_id.as_str(),
        "bound-preparation"
    );
    assert_eq!(repo.worker_demand().await.unwrap(), 1, "R1 no double count");
}

#[tokio::test]
async fn preparation_claim_advances_one_candidate_page_per_poll() {
    // Causes: C1 300 high-order explicit preparations require GPU; C2 the
    // registered Worker lacks GPU; C3 one compatible preparation sorts after
    // them; C4 a second, GPU-capable Worker starts after C2's first poll.
    // R1=C1+C2 -> the first 256-candidate poll returns no assignment;
    // R2=C1+C4 -> the GPU Worker starts at its own first page and claims its
    // head; R3=R1+C3 -> the first Worker resumes at page two and claims C3;
    // R4 follows end-of-index with a wrapped first page and no duplicate.
    // The cursor is a disposable per-incarnation hint;
    // every candidate still passes current-root and Worker eligibility checks.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    for index in 0..300 {
        create(
            repo.as_ref(),
            externally_placed(&format!("gpu-{index:03}"), "preparing", true),
        )
        .await;
    }
    create(
        repo.as_ref(),
        externally_placed("z-compatible", "preparing", true),
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
    observations.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(observations.clone())
        .unwrap();
    application
        .set_session_preparation_placement(Arc::new(SelectiveSessionPlacement))
        .unwrap();
    assert!(
        application
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "R1 bounded first page"
    );
    let gpu_target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "gpu-worker".into(),
        runtime_incarnation: "gpu-worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut gpu_manifest = terminal_cleanup_manifest();
    gpu_manifest.capabilities.insert("gpu".into());
    observations.replace(vec![
        terminal_cleanup_worker(&target, terminal_cleanup_manifest()),
        terminal_cleanup_worker(&gpu_target, gpu_manifest),
    ]);
    let gpu_claimed = application
        .claim_next_session_preparation(gpu_target)
        .await
        .unwrap()
        .expect("R2 independent GPU cursor");
    assert_eq!(gpu_claimed.session_id.as_str(), "gpu-000", "R2");
    let claimed = application
        .claim_next_session_preparation(target.clone())
        .await
        .unwrap()
        .expect("R3 compatible tail claim");
    assert_eq!(claimed.session_id.as_str(), "z-compatible", "R3");
    assert!(
        application
            .claim_next_session_preparation(target)
            .await
            .unwrap()
            .is_none(),
        "R4 wrapped page has only incompatible work"
    );
    assert_eq!(
        repo.get(&SessionId::from("gpu-001"))
            .await
            .unwrap()
            .revision
            .0,
        1,
        "R1 incompatible root unchanged"
    );
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
    // R2/R3/R5: malformed local or self-hosted run-free flags and an ordinary
    // Preparing root do not create Worker capacity demand; only R1 does.
    assert_eq!(repo.worker_demand().await.unwrap(), 1);
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
