use super::*;
use awaken_session_execution_contract::SessionPreparationControl;

struct FixtureSessionPlacement;

struct SelectiveSessionPlacement;

impl awaken_session_execution_contract::SessionPreparationPlacement for FixtureSessionPlacement {
    fn requirements(
        &self,
        _session: &PersistedSession,
    ) -> Result<
        awaken_worker_contract::PlacementRequirements,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        Ok(awaken_worker_contract::PlacementRequirements::remote_required())
    }
}

impl awaken_session_execution_contract::SessionPreparationPlacement for SelectiveSessionPlacement {
    fn requirements(
        &self,
        session: &PersistedSession,
    ) -> Result<
        awaken_worker_contract::PlacementRequirements,
        awaken_session_execution_contract::SessionRealizationControlFailure,
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
    if run_free {
        // W1: set the legal request through its existing assignment owner;
        // completed Idle history still keeps the flag without gaining demand.
        let mut aggregate = awaken_session_contract::SessionAggregate::try_from(session)
            .expect("valid external preparation fixture");
        aggregate
            .assignment()
            .request_run_free_preparation()
            .unwrap();
        session = aggregate.into_snapshot();
    }
    session
}

fn idle_mcp_update(id: &str, self_hosted: bool, bound: bool) -> PersistedSession {
    let mut root = externally_placed(id, "idle", false);
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut root.baseline
    else {
        panic!("frozen fixture");
    };
    baseline.environment.self_hosted = self_hosted;
    root.mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
        vec![crate::session_mcp::McpAttachmentDraft {
            name: "docs".into(),
            target: awaken_agent_contract::McpTarget::parse_http("https://docs.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin: crate::session_mcp::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    if bound {
        root.realization = Some(crate::session_resource::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            epoch: 3,
            expires_at_unix_ms: u64::MAX,
        });
    }
    root
}

#[test]
fn idle_mcp_preparation_uses_current_root_facts_without_a_second_request_flag() {
    // Cause/effect graph: C1 external Runtime placement; C2 Idle with no
    // activity; C3 unfinished MCP; C4 self-hosted Environment; C5 bound
    // realization owner; C6 terminal disposition. Effects: E1 candidate hint,
    // E2 excluded. Initial explicit opt-in rows remain in the existing tests.
    // R1 C1+C2+C3+!C4 -> E1 with or without C5; R2 same+C4+C5 -> E1;
    // R3 C4+!C5 -> E2; R4 !C1, R5 !Idle, R6 active activity, R7 C6,
    // R8 completed MCP -> E2. Every row keeps the initial request flag false.
    for (rule, self_hosted, bound, expected) in [
        ("R1 bound", false, true, true),
        ("R1 unbound", false, false, true),
        ("R2", true, true, true),
        ("R3", true, false, false),
        ("R4", false, true, false),
        ("R5", false, true, false),
        ("R6", false, true, false),
        ("R7", false, true, false),
        ("R8", false, true, false),
    ] {
        let mut root = idle_mcp_update(rule, self_hosted, bound);
        match rule {
            "R4" => {
                let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
                    &mut root.baseline
                else {
                    unreachable!();
                };
                baseline.runtime_placement = SessionRuntimePlacement::Local;
            }
            "R5" => root.execution = SessionExecutionState::Running,
            "R6" => {
                root.activity_epoch = 1;
                root.active_activity_epochs.insert(1);
            }
            "R7" => root.disposition = awaken_session_contract::SessionDisposition::Deleting,
            "R8" => {
                root.mcp.attachments[0].state = crate::session_mcp::McpAttachmentState::Active;
                root.mcp.attachments[0].publication_acknowledged = true;
            }
            _ => {}
        }
        assert_eq!(root.is_run_free_preparation_candidate(), expected, "{rule}");
        assert!(
            !root.realization_progress().run_free_initial_preparation(),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn idle_mcp_preparation_reuses_the_bound_worker_and_refuses_a_foreign_owner() {
    // Causes: C1 Idle MCP update, C2 cloud or already-bound self-hosted
    // Environment, C3 live Ready compatible Worker, C4 same or foreign owner.
    // R1=C1+C2+C3+same -> Stage the exact generation under the SAME lease;
    // R2=C1+C2+C3+foreign -> no assignment and byte-identical root. Neither
    // row creates a model Run, changes execution, or opts into initial work.
    for self_hosted in [false, true] {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let id = SessionId::from("idle-mcp-preparation");
        create(
            repo.as_ref(),
            idle_mcp_update(id.as_str(), self_hosted, true),
        )
        .await;
        let before = repo.get(&id).await.unwrap();
        let application = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let target = awaken_session_execution_contract::SessionRealizationTarget {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            lease_expires_at_unix_ms: u64::MAX,
            reassign_existing_lease: false,
        };
        let foreign = awaken_session_execution_contract::SessionRealizationTarget {
            owner: "foreign".into(),
            runtime_incarnation: "foreign:1:boot".into(),
            ..target.clone()
        };
        let observations = Arc::new(RecordingWorkerObservations::default());
        application
            .execution_admission()
            .set_worker_observation_source(observations.clone())
            .unwrap();
        application
            .execution_admission()
            .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
            .unwrap();
        observations.replace(vec![terminal_cleanup_worker(
            &foreign,
            terminal_cleanup_manifest(),
        )]);
        assert!(
            application
                .realizations
                .claim_next_session_preparation(foreign)
                .await
                .unwrap()
                .is_none(),
            "R2"
        );
        assert_eq!(repo.get(&id).await.unwrap(), before, "R2 root unchanged");
        observations.replace(vec![terminal_cleanup_worker(
            &target,
            terminal_cleanup_manifest(),
        )]);
        let assignment = application
            .realizations
            .claim_next_session_preparation(target)
            .await
            .unwrap()
            .expect("R1");
        assert_eq!(assignment.session_id, id, "R1 same Session");
        assert_eq!(
            Some(&assignment.directive.authority.lease),
            before.realization.as_ref(),
            "R1 same owner/epoch"
        );
        let awaken_session_execution_contract::SessionRealizationAction::Stage {
            mcp_stages, ..
        } = assignment.directive.action
        else {
            panic!("R1 exact pending Stage");
        };
        assert_eq!(mcp_stages.len(), 1, "R1");
        assert_eq!(
            mcp_stages[0].generation.generation(),
            &before.mcp.attachments[0].generation,
            "R1 exact generation"
        );
        let after = repo.get(&id).await.unwrap();
        assert_eq!(after.execution, before.execution, "R1 no model activity");
        assert_eq!(
            after.active_activity_epochs, before.active_activity_epochs,
            "R1"
        );
        assert!(
            !after.realization_progress().run_free_initial_preparation(),
            "R1 no new flag"
        );
        assert_eq!(
            repo.worker_demand().await.unwrap(),
            1,
            "R1 no extra replica"
        );
    }
}

#[tokio::test]
async fn run_free_preparation_is_worker_demand_before_any_run_or_owner() {
    // Causes: C1 an external Preparing root explicitly requested Run-free
    // preparation; C2 no realization owner or Dispatch exists; C3 the same
    // root has not requested preparation or has already reached Idle.
    // Effects: R1=C1+C2 contributes one unbound Worker demand, enough for a
    // zero-Worker fleet to wake; R2=C3 contributes zero. This is only a
    // capacity hint: the existing root-CAS claim still owns admission.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
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
            .needs_run_free_preparation_worker(),
        "R1 retained root classified before Worker registration",
    );
    assert!(
        !repo
            .get(&SessionId::from("ordinary"))
            .await
            .unwrap()
            .needs_run_free_preparation_worker(),
        "R2 ordinary Preparing root has no Run-free capacity request",
    );
    assert!(
        !repo
            .get(&SessionId::from("finished"))
            .await
            .unwrap()
            .needs_run_free_preparation_worker(),
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
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    for index in 0..300 {
        let mut root = externally_placed(&format!("retained-{index}"), "idle", false);
        root.realization = Some(crate::session_resource::SessionRealizationLease {
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
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
        .execution_admission()
        .set_worker_observation_source(observations)
        .unwrap();
    application
        .execution_admission()
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();
    let assignment = application
        .realizations
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
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut root = externally_placed("bound-preparation", "preparing", true);
    root.realization = Some(crate::session_resource::SessionRealizationLease {
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
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
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
        .execution_admission()
        .set_worker_observation_source(observations.clone())
        .unwrap();
    application
        .execution_admission()
        .set_session_preparation_placement(Arc::new(SelectiveSessionPlacement))
        .unwrap();
    assert!(
        application
            .realizations
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "R1 bounded first page"
    );
    let gpu_target = awaken_session_execution_contract::SessionRealizationTarget {
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
        .realizations
        .claim_next_session_preparation(gpu_target)
        .await
        .unwrap()
        .expect("R2 independent GPU cursor");
    assert_eq!(gpu_claimed.session_id.as_str(), "gpu-000", "R2");
    let claimed = application
        .realizations
        .claim_next_session_preparation(target.clone())
        .await
        .unwrap()
        .expect("R3 compatible tail claim");
    assert_eq!(claimed.session_id.as_str(), "z-compatible", "R3");
    assert!(
        application
            .realizations
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
async fn preparation_claim_does_not_starve_unvisited_candidates_after_a_retryable_head() {
    // Causes: C1 the first assigned root stays incomplete/retryable; C2 a
    // short final page or a full page has other compatible candidates; C3 the
    // same Worker polls again. Effects: E1 the next unvisited root is claimed,
    // E2 the unfinished head remains available on wrap, E3 no model Run.
    // R1 C1+C2(short)+C3 -> E1, not the same poison head; R2 C2(full) -> E1,
    // not the next PAGE's head; R3 end-of-index -> one empty poll then E2.
    // R4 two concurrent polls by this incarnation -> distinct first/second
    // roots under the same cursor lock and each root's ordinary CAS.
    // All rules use the real SQLite candidate index and root-CAS admission.
    // An assignment is deliberately not completed, matching a physical
    // preparation that cannot resolve an old credential after rotation.
    for (count, concurrent) in [(2, false), (300, false), (2, true)] {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        for index in 0..count {
            create(
                repo.as_ref(),
                externally_placed(&format!("candidate-{index:03}"), "preparing", true),
            )
            .await;
        }
        let page = repo.preparation_candidates(None).await.unwrap();
        assert_eq!(
            page.next_cursor.is_some(),
            count > 2,
            "R1/R2 page precondition"
        );
        let application = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let target = awaken_session_execution_contract::SessionRealizationTarget {
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
            .execution_admission()
            .set_worker_observation_source(observations)
            .unwrap();
        application
            .execution_admission()
            .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
            .unwrap();
        if concurrent {
            let (left, right) = tokio::join!(
                application
                    .realizations
                    .claim_next_session_preparation(target.clone()),
                application
                    .realizations
                    .claim_next_session_preparation(target.clone()),
            );
            let ids: std::collections::BTreeSet<_> = [left, right]
                .into_iter()
                .map(|result| {
                    result
                        .unwrap()
                        .expect("R4 assigned root")
                        .session_id
                        .to_string()
                })
                .collect();
            assert_eq!(
                ids,
                ["candidate-000".to_owned(), "candidate-001".to_owned()].into(),
                "R4"
            );
        } else {
            for index in 0..count.min(3) {
                let assignment = application
                    .realizations
                    .claim_next_session_preparation(target.clone())
                    .await
                    .unwrap()
                    .expect("R1/R2 another eligible root");
                assert_eq!(
                    assignment.session_id.as_str(),
                    format!("candidate-{index:03}"),
                    "R1/R2"
                );
            }
        }
        let head = repo.get(&SessionId::from("candidate-000")).await.unwrap();
        assert!(
            head.is_run_free_preparation_candidate(),
            "C1 remains retryable"
        );
        assert!(!head.has_active_activities(), "E3 no model activity");
        if count == 2 {
            assert!(
                application
                    .realizations
                    .claim_next_session_preparation(target.clone())
                    .await
                    .unwrap()
                    .is_none(),
                "R3 bounded end poll"
            );
            let wrapped = application
                .realizations
                .claim_next_session_preparation(target)
                .await
                .unwrap()
                .expect("R3 retry retained head");
            assert_eq!(wrapped.session_id.as_str(), "candidate-000", "R3");
        }
    }
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
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
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
    active.activity_epoch = 1;
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
        .realizations
        .request_run_free_initial_preparation(&id)
        .await
        .expect("R1/E1");
    assert!(
        first.realization_progress().run_free_initial_preparation(),
        "R1/E1"
    );
    assert!(first.revision.0 > before.revision.0, "R1 one root CAS");
    let replay = application
        .realizations
        .request_run_free_initial_preparation(&id)
        .await
        .expect("R2/E2");
    assert_eq!(replay.revision, first.revision, "R2/E2 no second CAS");
    for invalid in ["local-request", "self-hosted-request"] {
        let invalid = SessionId::from(invalid);
        assert!(
            matches!(
                application
                    .realizations
                    .request_run_free_initial_preparation(&invalid)
                    .await,
                Err(
                    awaken_session_execution_contract::SessionRealizationControlFailure::Invalid(_)
                )
            ),
            "R3/E3"
        );
        assert!(
            !repo
                .get(&invalid)
                .await
                .unwrap()
                .realization_progress()
                .run_free_initial_preparation(),
            "R3/E3"
        );
    }
    let completed = SessionId::from("completed");
    let before = repo.get(&completed).await.unwrap();
    let after = application
        .realizations
        .request_run_free_initial_preparation(&completed)
        .await
        .expect("R4/E4");
    assert_eq!(after.revision, before.revision, "R4/E4");
    assert!(
        !after.realization_progress().run_free_initial_preparation(),
        "R4/E4"
    );
    for blocked in ["terminated", "active"] {
        let blocked = SessionId::from(blocked);
        assert!(
            matches!(
                application
                    .realizations
                    .request_run_free_initial_preparation(&blocked)
                    .await,
                Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
            ),
            "R5/E5"
        );
        assert!(
            !repo
                .get(&blocked)
                .await
                .unwrap()
                .realization_progress()
                .run_free_initial_preparation(),
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
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut local = persisted("local", false, "preparing");
    // W5/R2: decoded malformed placement, not a newly admitted preparation.
    let mut local_progress = serde_json::to_value(&local).unwrap();
    local_progress["realization_progress"]["run_free_initial_preparation"] = true.into();
    local = serde_json::from_value(local_progress).unwrap();
    create(repo.as_ref(), local).await;
    let mut self_hosted = persisted("self-hosted", true, "preparing");
    // W5/R3: preserve the stored self-hosted opt-in for the original rejection.
    let mut self_hosted_progress = serde_json::to_value(&self_hosted).unwrap();
    self_hosted_progress["realization_progress"]["run_free_initial_preparation"] = true.into();
    self_hosted = serde_json::from_value(self_hosted_progress).unwrap();
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let observations = Arc::new(RecordingWorkerObservations::default());
    let eligible = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    observations.replace(vec![eligible.clone()]);
    application
        .execution_admission()
        .set_worker_observation_source(observations.clone())
        .unwrap();
    assert!(
        matches!(
            application
                .realizations
                .claim_next_session_preparation(target.clone())
                .await,
            Err(
                awaken_session_execution_contract::SessionRealizationControlFailure::Unavailable(_)
            )
        ),
        "R7 missing canonical compiler fails closed"
    );
    application
        .execution_admission()
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();

    let mut incapable = eligible.clone();
    incapable.snapshot.manifest.capacity.max_concurrent = 0;
    incapable.snapshot.capability_fingerprint = incapable.snapshot.manifest.fingerprint().unwrap();
    observations.replace(vec![incapable]);
    assert!(
        application
            .realizations
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
            .realizations
            .claim_next_session_preparation(target.clone())
            .await
            .unwrap()
            .is_none(),
        "R4/E2"
    );

    observations.replace(vec![eligible]);
    let assignment = application
        .realizations
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

    let another = awaken_session_execution_contract::SessionRealizationTarget {
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
            .realizations
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
    // to a physical Worker; the never-issued lease remains root evidence but is
    // explicitly closed, since no effect can run under it. R8=C1..C6 + C7 => E3.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(repo.as_ref(), externally_placed("race", "preparing", true)).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let target = awaken_session_execution_contract::SessionRealizationTarget {
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
        .execution_admission()
        .set_worker_observation_source(observations)
        .unwrap();
    application
        .execution_admission()
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();
    assert!(
        application
            .realizations
            .claim_next_session_preparation(target)
            .await
            .unwrap()
            .is_none(),
        "R8/E3"
    );
    let root = repo.get(&SessionId::from("race")).await.unwrap();
    assert_eq!(root.execution, SessionExecutionState::Preparing, "R8/E3");
    assert_eq!(
        root.realization
            .as_ref()
            .map(|lease| lease.expires_at_unix_ms),
        Some(0),
        "R8 the unissued lease stays as evidence but is explicitly closed"
    );
}

#[tokio::test]
async fn idle_unpublished_mcp_failures_end_by_class_and_budget() {
    // Regression for an Idle Run-free MCP update whose Active, unacknowledged
    // generation failed with an expired credential and was re-claimed 7,684
    // times, keeping its Worker leased. Causes: C1 failure class (terminal /
    // retryable / not-ready); C2 consecutive failures against a budget of 2.
    // Effects: E1 the generation stays Active and the root stays a candidate;
    // E2 the generation is Failed, the lease is explicitly closed (expiry 0)
    // and no Worker can claim the root again.
    // | Rule | C1         | C2               | effect |
    // | B1   | terminal   | first failure    | E2     |
    // | B2   | retryable  | below budget     | E1     |
    // | B3   | retryable  | budget reached   | E2     |
    // | B4   | not-ready  | any (4 rounds)   | E1 and nothing counted |
    for (rule, retryable, not_ready, rounds, fails) in [
        ("B1", false, false, 1, true),
        ("B2+B3", true, false, 2, true),
        ("B4", true, true, 4, false),
    ] {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let id = SessionId::from("idle-unpublished");
        let mut root = idle_mcp_update(id.as_str(), false, false);
        root.mcp.attachments[0].state = crate::session_mcp::McpAttachmentState::Active;
        root.mcp.attachments[0].publication_acknowledged = false;
        create(repo.as_ref(), root).await;
        let application = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                realization_retry_budget: 2,
                ..Default::default()
            },
        );
        let target = awaken_session_execution_contract::SessionRealizationTarget {
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
            .execution_admission()
            .set_worker_observation_source(observations)
            .unwrap();
        application
            .execution_admission()
            .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
            .unwrap();
        // The per-incarnation cursor advances past an examined root and wraps
        // on a later poll, so one claim may take up to two polls.
        let claim = |target: awaken_session_execution_contract::SessionRealizationTarget| {
            let application = &application;
            async move {
                for _ in 0..3 {
                    if let Some(assignment) = application
                        .realizations
                        .claim_next_session_preparation(target.clone())
                        .await
                        .unwrap()
                    {
                        return Some(assignment);
                    }
                }
                None
            }
        };
        for round in 1..=rounds {
            let assignment = claim(target.clone())
                .await
                .unwrap_or_else(|| panic!("{rule} round {round} claims the root"));
            application
                .realizations
                .fail_session_realization(
                    awaken_session_execution_contract::FailSessionRealization {
                        session_id: id.clone(),
                        lease: assignment.directive.authority.lease,
                        prepared_resource_revision: None,
                        retryable,
                        not_ready,
                        source_run_id: None,
                        reason: "sealed credential envelope is expired".into(),
                    },
                )
                .await
                .unwrap();
            let after = repo.get(&id).await.unwrap();
            let last = round == rounds;
            let expect_failed = fails && last;
            assert_eq!(
                after.mcp.attachments[0].state,
                if expect_failed {
                    crate::session_mcp::McpAttachmentState::Failed
                } else {
                    crate::session_mcp::McpAttachmentState::Active
                },
                "{rule} round {round}"
            );
            assert_eq!(
                after.is_run_free_preparation_candidate(),
                !expect_failed,
                "{rule} round {round} demand"
            );
            assert_eq!(
                after.realization.as_ref().unwrap().expires_at_unix_ms,
                0,
                "{rule} round {round} lease explicitly closed"
            );
            assert_eq!(
                after.execution,
                SessionExecutionState::Idle,
                "{rule} stays Idle"
            );
            assert_eq!(
                after.realization_progress().uncounted_failures(),
                if not_ready {
                    0
                } else {
                    u32::try_from(round).unwrap()
                },
                "{rule} round {round} counted"
            );
        }
        let after = repo.get(&id).await.unwrap();
        assert_eq!(claim(target).await.is_some(), !fails, "{rule} re-claim");
        if fails {
            assert_eq!(
                repo.get(&id).await.unwrap(),
                after,
                "{rule} exhausted root is not touched again"
            );
            assert_eq!(
                after.mcp.attachments[0].last_error.as_deref(),
                Some("sealed credential envelope is expired"),
                "{rule}"
            );
        }
    }
}

#[tokio::test]
async fn preparation_closes_its_unissued_lease_when_the_worker_starts_draining() {
    // R8b: the Worker turns Draining between the first eligibility read and
    // the post-CAS re-check. No directive leaves the claim, so no effect can
    // run; the minted lease is explicitly closed (expiry 0) rather than left
    // live on a retiring Worker, where an only time-expired lease would keep
    // its retirement refused. The lease identity stays as root evidence.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(
        repo.as_ref(),
        externally_placed("drain-race", "preparing", true),
    )
    .await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let eligible = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    let mut draining = eligible.clone();
    draining.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    let observations = Arc::new(RecordingWorkerObservations::default());
    observations.script([
        WorkerObservationStep::Workers(vec![eligible]),
        WorkerObservationStep::Workers(vec![draining]),
    ]);
    application
        .execution_admission()
        .set_worker_observation_source(observations)
        .unwrap();
    application
        .execution_admission()
        .set_session_preparation_placement(Arc::new(FixtureSessionPlacement))
        .unwrap();
    assert!(
        application
            .realizations
            .claim_next_session_preparation(target)
            .await
            .unwrap()
            .is_none(),
        "R8b no directive"
    );
    let root = repo.get(&SessionId::from("drain-race")).await.unwrap();
    let lease = root
        .realization
        .as_ref()
        .expect("R8b lease evidence retained");
    assert_eq!(lease.owner, "worker", "R8b exact attempted owner");
    assert_eq!(lease.expires_at_unix_ms, 0, "R8b explicitly closed");
    assert_eq!(root.execution, SessionExecutionState::Preparing, "R8b");
}
