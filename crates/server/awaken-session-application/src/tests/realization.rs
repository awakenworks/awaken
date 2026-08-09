use super::*;

#[tokio::test]
async fn local_realization_uses_stable_owner_and_process_incarnation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("local-restart", false, false, "idle"),
    )
    .await;
    let configured = |owner: &str| SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::LocalWorker,
        local_realization_owner: owner.into(),
    };

    let first = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let first_session = first.realize_session("local-restart").await.expect("L1");
    let first_lease = first_session.realization.expect("L1 lease");
    assert_eq!(first_lease.owner, "embedded-worker", "L1/E1");
    assert_eq!(
        first
            .realize_session("local-restart")
            .await
            .expect("L1 exact replay")
            .realization
            .expect("L1 replay lease"),
        first_lease,
        "L1/E1"
    );

    let replacement = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let replaced = replacement
        .realize_session("local-restart")
        .await
        .expect("L2 same logical owner may fence a crashed incarnation");
    let replaced_lease = replaced.realization.expect("L2 lease");
    assert_eq!(replaced_lease.owner, first_lease.owner, "L2/E2");
    assert_ne!(
        replaced_lease.runtime_incarnation, first_lease.runtime_incarnation,
        "L2/E2"
    );
    assert_eq!(replaced_lease.epoch, first_lease.epoch + 1, "L2/E2");

    let other = application_with_configuration(
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        configured("other-worker"),
    );
    assert!(
        matches!(
            other.realize_session("local-restart").await,
            Err(crate::SessionRealizationError::Control(
                awaken_session_contract::SessionRealizationControlFailure::StaleOwnership
            ))
        ),
        "L3/E3"
    );
    let reassigned = awaken_session_contract::SessionRealizationControl::begin_session_realization(
        &other,
        awaken_session_contract::BeginSessionRealization {
            session_id: "local-restart".into(),
            target: awaken_session_contract::SessionRealizationTarget {
                owner: "other-worker".into(),
                runtime_incarnation: "other-worker/boot-2".into(),
                lease_expires_at_unix_ms: u64::MAX,
                renew_existing_lease: false,
                reassign_existing_lease: true,
            },
        },
    )
    .await
    .expect("L4 claim-authorized reassignment");
    assert_eq!(reassigned.lease.epoch, replaced_lease.epoch + 1, "L4/E4");
    assert_eq!(reassigned.lease.owner, "other-worker", "L4/E4");

    let invalid = awaken_session_contract::SessionRealizationControl::begin_session_realization(
        &other,
        awaken_session_contract::BeginSessionRealization {
            session_id: "local-restart".into(),
            target: awaken_session_contract::SessionRealizationTarget {
                owner: "other-worker".into(),
                runtime_incarnation: "other-worker/boot-2".into(),
                lease_expires_at_unix_ms: u64::MAX,
                renew_existing_lease: true,
                reassign_existing_lease: true,
            },
        },
    )
    .await;
    assert!(
        matches!(
            invalid,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "L5"
    );
}

/// Reference-projection FMECA and cause/effect decision table. C1 the
/// Resource index accepts or rejects a replacement; C2 the Session root CAS
/// wins or conflicts after projection. Effects are E1 no durable intent is
/// exposed without its safety edge, E2 a losing candidate is repaired back
/// to repository truth, and E3 a winning pending replacement retains both
/// installed and desired targets until settlement.
///
/// | Rule | C1 index | C2 root CAS | Effect |
/// |---|---|---|---|
/// | R1 | rejects | not attempted | E1 unchanged Session |
/// | R2 | accepts | conflicts | E2 installed target only |
/// | R3 | accepts | wins | E3 installed + desired targets |
#[tokio::test]
async fn resource_reference_projection_fails_closed_and_repairs_cas_losers() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut application = application(repo.clone(), environments);
    let references = Arc::new(RecordingReferenceIndex::default());
    application.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));

    let mut original = persisted("reference-fence", false, false, "idle");
    original.resources =
        awaken_session_contract::SessionResourceState::from_legacy(skill_resources("old"));
    create(repo.as_ref(), original).await;

    let mut rejected = repo.get("reference-fence").await.unwrap();
    let original_revision = rejected.revision;
    rejected
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    references.fail_replace.store(true, Ordering::SeqCst);
    assert!(
        application
            .commit_resource_snapshot("workspace", rejected, "R1", Vec::new())
            .await
            .is_err(),
        "R1"
    );
    assert_eq!(
        repo.get("reference-fence").await.unwrap().revision,
        original_revision,
        "R1/E1"
    );

    references.fail_replace.store(false, Ordering::SeqCst);
    let mut stale = repo.get("reference-fence").await.unwrap();
    let mut winner = stale.clone();
    winner.title = Some("concurrent winner".into());
    application
        .commit_session_snapshot("workspace", winner, "concurrent", Vec::new())
        .await
        .unwrap();
    stale
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    assert!(matches!(
        application
            .commit_resource_snapshot("workspace", stale, "R2", Vec::new())
            .await,
        Err(SessionMutationError::Conflict)
    ));
    let ids = || {
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(ids(), BTreeSet::from(["old".to_string()]), "R2/E2");

    let mut accepted = repo.get("reference-fence").await.unwrap();
    accepted
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    application
        .commit_resource_snapshot("workspace", accepted, "R3", Vec::new())
        .await
        .unwrap();
    assert_eq!(
        ids(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R3/E3"
    );
}

/// Legacy Resource adoption FMECA and cause/effect decision table. C1 the
/// retained manifest has no activation records; C2 the Worker reports that
/// it prepared the exact active revision; C3 the reported revision is stale.
/// Effects are E1 one Active/attempted record is committed, E2 no second
/// physical-state authority is created, and E3 a mismatched generation is
/// rejected without mutating durable truth.
///
/// | Rule | Legacy inputs | Prepared revision | Effect |
/// |---|---|---|---|
/// | L1 | present | exact | E1 adopt once |
/// | L2 | present | stale | E3 reject, unchanged |
/// | L3 | absent/already adopted | any | E2 normal realization path |
#[tokio::test]
async fn remote_realization_adopts_only_the_exact_legacy_resource_generation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    for id in ["legacy-exact", "legacy-stale"] {
        let mut session = persisted(id, false, false, "preparing");
        session.resources =
            awaken_session_contract::SessionResourceState::from_legacy(file_resources(id));
        session.realization = Some(lease.clone());
        create(repo.as_ref(), session).await;
    }

    let exact = awaken_session_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_contract::ActivateSessionRealization {
            session_id: "legacy-exact".into(),
            lease: lease.clone(),
            prepared_resource_revision: Some(1),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect("L1 exact legacy generation");
    assert!(
        matches!(
            exact.action,
            awaken_session_contract::SessionRealizationAction::Publish { .. }
        ),
        "L1/E1"
    );
    let adopted = repo.get("legacy-exact").await.expect("L1 durable truth");
    assert_eq!(adopted.resources.activations.len(), 1, "L1/E1");
    assert_eq!(
        adopted.resources.activations[0].state,
        awaken_session_contract::ActivationState::Active,
        "L1/E1"
    );
    assert_eq!(adopted.resources.activations[0].attempts, 1, "L1/E1");

    let stale = awaken_session_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_contract::ActivateSessionRealization {
            session_id: "legacy-stale".into(),
            lease,
            prepared_resource_revision: Some(2),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect_err("L2 stale generation must fail closed");
    assert!(
        matches!(
            stale,
            awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "L2/E3: {stale}"
    );
    assert!(
        repo.get("legacy-stale")
            .await
            .expect("L2 durable truth")
            .resources
            .activations
            .is_empty(),
        "L2/E3"
    );
}

/// Terminal Worker-placement FMECA and cause/effect decision table. C1 the
/// Session placement delegates live realization to a Worker; C2 the Session
/// is live with pending work; C3 it is terminal with Releasing work. Effects
/// are E1 no Coordinator execution effect for a live generation and E2 the
/// sole terminal cleanup path converges because no future Run can claim it.
///
/// | Rule | Placement | Lifecycle | Resource state | Effect |
/// |---|---|---|---|---|
/// | W1 | Worker | idle | Prepared | E1 remain unattempted |
/// | W2 | Worker | terminal | Releasing | E2 release durably |
#[tokio::test]
async fn worker_placement_defers_live_effects_but_not_terminal_cleanup() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let worker_baseline = |id: &str, status: &str| {
        let mut session = persisted(id, false, false, status);
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = SessionRuntimePlacement::Worker;
        session
    };

    let mut live = worker_baseline("worker-live", "idle");
    live.resources
        .prepare("worker-live", file_resources("live"))
        .expect("W1 prepared generation");
    create(repo.as_ref(), live).await;

    let mut terminal = worker_baseline("worker-terminal", "terminated");
    terminal.resources =
        awaken_session_contract::SessionResourceState::from_legacy(file_resources("terminal"));
    terminal.resources.adopt_legacy_active("worker-terminal");
    terminal
        .resources
        .begin_release()
        .expect("W2 release intent");
    create(repo.as_ref(), terminal).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let report = application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "W1/W2: {:#?}", report.failures);
    assert_eq!(report.settled.len(), 1, "W2/E2");

    let live = repo.get("worker-live").await.expect("W1 durable truth");
    assert!(live.resources.pending.is_some(), "W1/E1");
    assert_eq!(live.resources.activations[0].attempts, 0, "W1/E1");
    let terminal = repo.get("worker-terminal").await.expect("W2 durable truth");
    assert!(
        terminal.resources.activations.iter().all(
            |activation| activation.state == awaken_session_contract::ActivationState::Released
        ),
        "W2/E2 durable={:?} settled={:?}",
        terminal.resources.activations,
        report.settled[0].resources.activations
    );
}

/// Crash-window matrix for terminal effects. R1 persists the intent before
/// an injected Runtime failure; R2 recovery reuses the exact effect id and
/// commits the receipt plus Environment removal atomically; R3 a later
/// reconciler observes Completed and performs no second effect.
#[tokio::test]
async fn terminal_cleanup_recovery_reuses_intent_and_skips_completed_effects() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("cleanup-recovery", false, false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.fail_once.store(true, Ordering::SeqCst);
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect_err("R1 injected crash leaves cleanup pending");
    let pending = repo
        .get("cleanup-recovery")
        .await
        .expect("R1 durable intent");
    assert!(pending.terminal_cleanup.is_requested(), "R1 intent");
    assert_eq!(
        pending.environment.binding(),
        Some("opaque-environment"),
        "R1 Environment remains authoritative until receipt commit"
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R2 recovery")
        .expect("R2 retained Session");
    assert!(completed.terminal_cleanup.is_completed(), "R2 receipt");
    assert!(
        matches!(
            completed.environment,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
        ),
        "R2 Environment projection removed with receipt"
    );

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R3 completed replay");
    let intents = runtime.intents.lock().unwrap();
    assert_eq!(intents.len(), 2, "R3 performs no effect");
    assert_eq!(
        intents[0].effect_id, intents[1].effect_id,
        "R1/R2 stable intent"
    );
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "R1/R2");
}

/// Complete durable terminal-cleanup crash matrix. Causes are the process stop
/// location relative to C1=fence CAS, C2=quiesce, C3=target-intent CAS,
/// C4=idempotent external effect, C5=exact receipt, and C6=completion CAS.
/// Effects are E1 no pre-intent I/O, E2 recover from the last committed phase,
/// E3 reuse one effect identity, E4 keep Environment authority until completion,
/// and E5 replay Completed without another effect.
///
/// | Rule | Crash point | Durable phase | Recovery effect |
/// |---|---|---|---|
/// | F0 | before C1 | NotRequested | E1; normal fence begins later |
/// | F1 | after C1/before C2 | Fenced | E2 quiesces before targets |
/// | F2 | after C2/before C3 | Fenced | E2 repeats quiesce, freezes once |
/// | F3 | after C3/before C4 | Requested | E2 executes exact frozen targets |
/// | F4 | after C4/before C5 | Requested | E3 idempotent effect replay |
/// | F5 | after C5/before C6 | Requested | E3 receipt replay, E4 retained |
/// | F6 | after C6/before response | Completed | E5 no second effect |
#[tokio::test]
async fn terminal_cleanup_f0_through_f6_recover_from_durable_phase() {
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-f0-f6", false, false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let f0 = repo.get("cleanup-f0-f6").await.expect("F0 truth");
    assert!(!f0.terminal_cleanup.needs_reconciliation(), "F0/E1");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F0/E1");

    runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F1 crash before quiescence");
    let f1 = repo.get("cleanup-f0-f6").await.expect("F1 truth");
    assert!(f1.terminal_cleanup.is_fenced(), "F1/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F1/E1");

    repo.fail_once("resource-release-intent");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F2 target-intent CAS outage after quiescence");
    let f2 = repo.get("cleanup-f0-f6").await.expect("F2 truth");
    assert!(f2.terminal_cleanup.is_fenced(), "F2/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F2/E1");

    runtime
        .fail_before_effect_once
        .store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F3 crash after target freeze before effect");
    let f3 = repo.get("cleanup-f0-f6").await.expect("F3 truth");
    assert!(f3.terminal_cleanup.is_requested(), "F3/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F3/E1");
    assert!(runtime.effective_ids.lock().unwrap().is_empty(), "F3/E1");

    runtime.fail_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F4 lost effect receipt");
    let f4 = repo.get("cleanup-f0-f6").await.expect("F4 truth");
    assert!(f4.terminal_cleanup.is_requested(), "F4/E2");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F4/E3");
    assert_eq!(
        f4.environment.binding(),
        Some("opaque-environment"),
        "F4/E4"
    );

    repo.fail_once("resource-release-complete");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F5 completion CAS outage");
    let f5 = repo.get("cleanup-f0-f6").await.expect("F5 truth");
    assert!(f5.terminal_cleanup.is_requested(), "F5/E2");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F5/E3");
    assert_eq!(
        f5.environment.binding(),
        Some("opaque-environment"),
        "F5/E4"
    );

    let f6 = application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 recovery")
        .expect("archived Session retained");
    assert!(f6.terminal_cleanup.is_completed(), "F6/E5");
    assert!(matches!(
        f6.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
    let attempts = runtime.intents.lock().unwrap().len();
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 response-loss replay");
    assert_eq!(runtime.intents.lock().unwrap().len(), attempts, "F6/E5");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F6/E5");
}

/// Deterministic archive/delegation race: the cleanup worker is paused after
/// the durable terminal fence but before its delegation snapshot. A child
/// committed in that interval must be included in the frozen target set and
/// can never be lost behind a premature Completed marker.
#[tokio::test]
async fn terminal_fence_quiesces_before_freezing_concurrent_child() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("cleanup-concurrent-child", false, false, "terminated"),
    )
    .await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.block_quiesce.store(true, Ordering::SeqCst);
    let application = Arc::new(SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    ));

    let cleanup = {
        let application = application.clone();
        tokio::spawn(async move {
            application
                .release_terminal_resources("workspace", "cleanup-concurrent-child")
                .await
        })
    };
    runtime.quiesce_entered.notified().await;
    let fenced = repo
        .get("cleanup-concurrent-child")
        .await
        .expect("terminal fence committed before quiescence");
    assert!(fenced.terminal_cleanup.is_fenced());

    *runtime.delegated_snapshot.lock().unwrap() = awaken_session_contract::DelegatedRunSnapshot {
        delegated_runs: vec![awaken_session_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-after-fence".into()),
            parent_call_id: "delegate-call".into(),
            agent_id: "child-agent".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        }],
        watermark: 23,
    };
    runtime.quiesce_release.notify_one();

    let completed = cleanup
        .await
        .expect("cleanup task")
        .expect("cleanup succeeds")
        .expect("archived Session remains");
    let awaken_session_contract::SessionTerminalCleanupState::Completed {
        thread_ids,
        delegation_watermark,
        ..
    } = completed.terminal_cleanup
    else {
        panic!("terminal cleanup must be completed");
    };
    assert_eq!(delegation_watermark, 23);
    assert_eq!(
        thread_ids,
        BTreeSet::from([
            "cleanup-concurrent-child".to_string(),
            "child-after-fence".to_string(),
        ])
    );
    let cleaned = runtime
        .intents
        .lock()
        .unwrap()
        .iter()
        .map(|intent| intent.thread_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(cleaned, thread_ids);
}

#[tokio::test]
async fn terminal_cleanup_restart_soak_preserves_authority_and_effect_identity() {
    /* Soak cause/effect model. For each of 64 independent Sessions: C1 a
     * delegated child exists in durable Runtime truth; C2 terminal cleanup
     * completes; C3 the Coordinator/Store is reopened and the command is
     * replayed. Effects: E1 root+child are frozen exactly once; E2 one stable
     * effective cleanup per thread; E3 restart replay performs no new I/O; E4
     * no pending/quarantined Session authority leaks across iterations. The
     * separate blocked-quiescence test owns the concurrent interleaving oracle;
     * this loop owns repetition and restart leakage. */
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup-soak.db");
    let path = path.to_string_lossy().to_string();
    for iteration in 0..64 {
        let session_id = format!("cleanup-soak-{iteration}");
        let child_id = format!("cleanup-soak-child-{iteration}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("open Session authority"),
        );
        create(
            repo.as_ref(),
            persisted(&session_id, false, false, "terminated"),
        )
        .await;
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.delegated_snapshot.lock().unwrap() =
            awaken_session_contract::DelegatedRunSnapshot {
                delegated_runs: vec![awaken_session_contract::DelegatedRun {
                    run_id: awaken_agent_contract::agent::run::Id(child_id.clone()),
                    parent_call_id: format!("delegate-{iteration}"),
                    agent_id: "child-agent".into(),
                    status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
                }],
                watermark: iteration + 1,
            };
        let application = SessionApplication::new_with_configuration(
            runtime.clone(),
            Arc::new(NoopMcpRealizer),
            repo,
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        let completed = application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.terminal_cleanup.thread_ids().unwrap(),
            &BTreeSet::from([session_id.clone(), child_id]),
            "E1 iteration {iteration}",
        );
        assert_eq!(runtime.effective_ids.lock().unwrap().len(), 2, "E2");

        drop(application);
        let reopened = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("restart Session authority"),
        );
        let replay_runtime = Arc::new(RecordingCleanupRuntime::default());
        let replay_application = SessionApplication::new_with_configuration(
            replay_runtime.clone(),
            Arc::new(NoopMcpRealizer),
            reopened.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        replay_application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap();
        assert!(replay_runtime.intents.lock().unwrap().is_empty(), "E3");
        let scan = reopened.reconcilable_sessions().await.unwrap();
        assert!(scan.quarantined.is_empty(), "E4");
        assert!(
            scan.sessions
                .iter()
                .all(|row| row.session.session_id != session_id),
            "E4",
        );
    }
}

/// Cause/effect graph: C1 a baseline is frozen; C2 its explicit Runtime
/// placement is Local or Worker; C3 a retained pre-placement row is marked
/// LegacyUnspecified; C4 the process composition is local or registered;
/// C5 an application contribution independently requires Worker custody.
/// The realization lease is intentionally absent from the causes: it is an
/// assignment fence, never placement policy. Effects are E1 local physical
/// realization or E2 dispatch-only Coordinator projection.
///
/// | Rule | Frozen placement | Process placement | Application | Effect |
/// |---|---|---|---|---|
/// | P1 | preparing | any | n/a | E1 (not yet realizable) |
/// | P2 | local | local/registered | absent | E1 |
/// | P3 | worker | local/registered | absent | E2 |
/// | P4 | local/worker | any | present | E2 |
/// | P5 | legacy | local | absent | E1 |
/// | P6 | legacy | registered | absent | E2 |
///
/// P5/P6 are the one-way upgrade interpretation for rows serialized before
/// placement existed. New creation is separately asserted never to emit the
/// legacy value.
#[test]
fn realization_owner_follows_the_application_placement_decision_table() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let local = application_with_configuration(
        repo.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            ..Default::default()
        },
    );
    let registered = application_with_configuration(
        repo,
        environments,
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );

    let frozen = |placement: SessionRuntimePlacement, application: bool| {
        let mut value = persisted("placement", false, false, "idle");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut value.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = placement;
        baseline.application =
            application.then(|| awaken_session_contract::ApplicationContributionReceipt {
                plan_fingerprint: "plan".into(),
                input_fingerprint: "input".into(),
            });
        value
    };
    let preparing = {
        let mut value = frozen(SessionRuntimePlacement::Local, false);
        let baseline = value.frozen_baseline().expect("frozen").clone();
        value.baseline = awaken_session_contract::SessionBaselineState::Preparing(
            awaken_session_contract::SessionCreationIntent {
                control: awaken_session_contract::ControlSessionCreationInputs {
                    environment: baseline.environment,
                    runtime_placement: SessionRuntimePlacement::Local,
                    agent_id: baseline.agent_id,
                    model: baseline.model,
                    execution_model_ref: baseline.execution_model_ref,
                    runtime: baseline.runtime,
                    mcp_authoring: baseline.mcp_authoring,
                    delegate_ids: baseline.delegate_ids,
                    toolsets: baseline.toolsets,
                    mounts: baseline.mounts,
                    env: baseline.env,
                    prompts: baseline.prompts,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
                application: awaken_session_contract::ApplicationContributionState::Absent,
            },
        );
        value
    };

    for (rule, value, local_expected, registered_expected) in [
        ("P1", preparing, false, false),
        (
            "P2",
            frozen(SessionRuntimePlacement::Local, false),
            false,
            false,
        ),
        (
            "P3",
            frozen(SessionRuntimePlacement::Worker, false),
            true,
            true,
        ),
        (
            "P4a",
            frozen(SessionRuntimePlacement::Local, true),
            true,
            true,
        ),
        (
            "P4b",
            frozen(SessionRuntimePlacement::Worker, true),
            true,
            true,
        ),
        (
            "P5/P6",
            frozen(SessionRuntimePlacement::LegacyUnspecified, false),
            false,
            true,
        ),
    ] {
        assert_eq!(
            local.requires_external_realization(&value),
            local_expected,
            "{rule}/local"
        );
        assert_eq!(
            registered.requires_external_realization(&value),
            registered_expected,
            "{rule}/registered"
        );
    }
    assert_eq!(local.runtime_placement(), SessionRuntimePlacement::Local);
    assert_eq!(
        registered.runtime_placement(),
        SessionRuntimePlacement::Worker
    );
}

#[tokio::test]
async fn publication_acknowledgement_follows_the_renewal_decision_table() {
    // Cause/effect graph: C1 acknowledgement names the exact active
    // generation; C2 it names the same owner/incarnation/epoch under a
    // shorter expiry; C3 any immutable generation coordinate differs.
    // Effects: E1 acknowledge and make the Session idle; E2 commit nothing
    // and return the current Stage for canonical catch-up; E3 fail closed
    // without changing publication state.
    //
    // | Rule | exact | monotonic predecessor | replacement | Effect |
    // |---|---|---|---|---|
    // | A1 | yes | no | no | E1 |
    // | A2 | no | yes | no | E2 |
    // | A3 | no | no | yes | E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = application(repo.clone(), environments);
    let current_expiry = u64::MAX - 1;

    let fixture = |id: &str| {
        let mut session = persisted(id, false, false, "activating");
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: current_expiry,
        };
        let attachment_id = awaken_session_contract::McpAttachmentId("browser".into());
        let generation = awaken_session_contract::McpGeneration(1);
        session.realization = Some(lease.clone());
        session
            .mcp
            .attachments
            .push(awaken_session_contract::SessionMcpAttachment {
                attachment_id: attachment_id.clone(),
                name: "browser".into(),
                generation,
                target: awaken_session_contract::McpTarget::parse_http(
                    "https://browser.example.test/mcp",
                )
                .expect("MCP target"),
                prompts_as_skills: false,
                origin: awaken_session_contract::McpAttachmentOrigin::Agent,
                credential: None,
                selected_plaintext_holder: None,
                state: awaken_session_contract::McpAttachmentState::Active,
                publication_acknowledged: false,
                realization: Some(awaken_session_contract::McpRealizationClaim {
                    realization_id: "realize-browser-1".into(),
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_epoch: lease.epoch,
                    lease_expires_at_unix_ms: current_expiry,
                    stage_idempotency_key: "renew-browser-1".into(),
                }),
                attempts: 1,
                last_error: None,
            });
        let generation_ref = awaken_session_contract::McpGenerationRef {
            session_id: id.into(),
            attachment_id,
            generation,
            runtime_incarnation: lease.runtime_incarnation.clone(),
            lease_epoch: lease.epoch,
            lease_expires_at_unix_ms: current_expiry,
        };
        (session, lease, generation_ref)
    };

    let (exact, exact_lease, exact_generation) = fixture("ack-exact");
    create(repo.as_ref(), exact).await;
    let exact_result =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-exact".into(),
                lease: exact_lease,
                published: vec![exact_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A1 exact acknowledgement");
    assert!(
        matches!(
            exact_result.action,
            awaken_session_contract::SessionRealizationAction::Complete
        ),
        "A1/E1"
    );
    assert_eq!(
        repo.get("ack-exact").await.unwrap().execution.as_str(),
        "idle",
        "A1/E1"
    );

    let (renewed, renewed_lease, mut predecessor) = fixture("ack-renewed");
    create(repo.as_ref(), renewed).await;
    predecessor.lease_expires_at_unix_ms -= 1;
    let mut predecessor_lease = renewed_lease.clone();
    predecessor_lease.expires_at_unix_ms -= 1;
    let renewed_result =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-renewed".into(),
                lease: predecessor_lease,
                published: vec![predecessor],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A2 monotonic predecessor");
    assert!(
        matches!(
            renewed_result.action,
            awaken_session_contract::SessionRealizationAction::Stage { .. }
        ),
        "A2/E2"
    );
    let renewed_truth = repo.get("ack-renewed").await.unwrap();
    assert_eq!(renewed_truth.execution.as_str(), "activating", "A2/E2");
    assert!(
        !renewed_truth.mcp.attachments[0].publication_acknowledged,
        "A2/E2"
    );

    let (replacement, replacement_lease, mut wrong_generation) = fixture("ack-replaced");
    create(repo.as_ref(), replacement).await;
    wrong_generation.attachment_id = awaken_session_contract::McpAttachmentId("other".into());
    let error =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-replaced".into(),
                lease: replacement_lease,
                published: vec![wrong_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect_err("A3 replacement must fail closed");
    assert!(
        matches!(
            error,
            awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "A3/E3: {error}"
    );
    assert!(
        !repo.get("ack-replaced").await.unwrap().mcp.attachments[0].publication_acknowledged,
        "A3/E3"
    );
}
