use super::*;

#[tokio::test]
async fn independently_exited_predecessor_allows_terminal_assignment_without_cleanup_ack() {
    // Minimal affinity regression: C1 a terminal root has frozen root-only
    // cleanup and an unchanged physical binding; C2 its lease is expired;
    // C3 Registry retains the exact predecessor's independent process exit;
    // C4 the replacement is a live, compatible Registry incarnation.
    // R0 same-incarnation control proves the remaining claim prerequisites;
    // R1 foreign+C1+C2+C3+C4 -> only a higher-epoch terminal assignment.
    // Negative rules independently remove frozen scope, lease expiry, exact
    // closed/valid exit evidence, or make its read unavailable. Every negative
    // preserves the whole root; time or a different boot never grants a claim.
    // E1 preserve original binding/cleanup identity and every preparation
    // obligation; E2 never infer no-Create, dispose, publish or acknowledge an
    // old physical attempt. The deliberately opaque handle is not adopted by
    // this test: the physical provider must still validate it after assignment.
    let now = awaken_time::system_clock().now().as_epoch_ms();
    let replacement =
        awaken_worker_contract::WorkerIdentity::new("replacement-worker", "new-boot", 1);
    let previous = awaken_worker_contract::WorkerIdentity::new("previous-worker", "old-boot", 1);
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: replacement.worker_id.clone(),
        runtime_incarnation: replacement.lease_owner(),
        lease_expires_at_unix_ms: now.saturating_add(60_000),
        reassign_existing_lease: false,
    };
    for (rule, predecessor, allowed) in [
        ("R0", &replacement, true),
        ("R1", &previous, true),
        ("missing-exit", &previous, false),
        ("live", &previous, false),
        ("unfrozen", &previous, false),
        ("active-record", &previous, false),
        ("foreign-incarnation", &previous, false),
        ("invalid-exit", &previous, false),
        ("read-error", &previous, false),
        ("old-decoder", &previous, false),
    ] {
        let repo = sqlite_session_repository("exact predecessor exit cleanup assignment").await;
        let session_id = format!("terminal-exited-predecessor-{rule}");
        let old_lease = crate::session_resource::SessionRealizationLease {
            owner: predecessor.worker_id.clone(),
            runtime_incarnation: predecessor.lease_owner(),
            epoch: 7,
            expires_at_unix_ms: if rule == "live" { u64::MAX } else { 1 },
        };
        let mut root = registry_race_terminal_session(&session_id, Some(old_lease.clone()));
        root.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
                "fixture",
                format!("bound-{session_id}"),
            ))
            .unwrap(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let original_environment = root.environment.clone();
        if rule == "unfrozen" {
            root.terminal_cleanup = Default::default();
            assert!(root.ensure_terminal_cleanup_fence());
        }
        let original_cleanup = root.terminal_cleanup.clone();
        create(repo.as_ref(), root).await;
        let before = repo
            .get(&SessionId::from(session_id.as_str()))
            .await
            .unwrap();
        let workers = Arc::new(RecordingWorkerObservations::default());
        let mut active = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
        if rule == "old-decoder" {
            active
                .snapshot
                .manifest
                .capabilities
                .remove(awaken_worker_contract::SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY);
            active.snapshot.capability_fingerprint =
                active.snapshot.manifest.fingerprint().unwrap();
        }
        let old_target = awaken_session_realization_contract::SessionRealizationTarget {
            owner: previous.worker_id.clone(),
            runtime_incarnation: previous.lease_owner(),
            lease_expires_at_unix_ms: 1,
            reassign_existing_lease: false,
        };
        let mut exited = terminal_cleanup_worker(&old_target, terminal_cleanup_manifest());
        exited.snapshot.state = awaken_worker_contract::WorkerState::Dead;
        exited.process_exit = Some(
            awaken_worker_contract::WorkerProcessExit::new(
                "fixture",
                "original-worker-process",
                "independent-exit",
            )
            .unwrap(),
        );
        if rule == "missing-exit" {
            exited.process_exit = None;
        }
        if rule == "active-record" {
            exited.snapshot.state = awaken_worker_contract::WorkerState::Ready;
        }
        if rule == "foreign-incarnation" {
            exited.snapshot.identity.incarnation_id = "unrelated-boot".into();
        }
        if rule == "invalid-exit" {
            exited.process_exit = Some(serde_json::from_value(serde_json::json!({"provider_id":"", "execution_ref":"original-worker-process", "evidence_ref":"independent-exit"})).unwrap());
        }
        workers
            .fail_incarnation
            .store(rule == "read-error", Ordering::SeqCst);
        workers.replace(vec![active, exited]);
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        let app = configured_application(
            runtime.clone(),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
        );
        app.execution_admission()
            .set_worker_observation_source(workers)
            .unwrap();
        let observed = awaken_session_realization_contract::SessionTerminalCleanupQueue::claim_next_terminal_cleanup(&app, target.clone(), None).await;
        if !allowed {
            if rule == "read-error" {
                assert!(matches!(observed, Err(awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_))), "{rule}");
            } else {
                assert!(observed.unwrap().is_none(), "{rule}");
            }
            assert_eq!(
                repo.get(&SessionId::from(session_id.as_str()))
                    .await
                    .unwrap(),
                before,
                "{rule}: zero root mutation"
            );
            continue;
        }
        let assignment = observed.unwrap();
        let assignment = assignment.unwrap_or_else(|| {
            panic!("{rule}: original bound cleanup must be claimable after exact independent exit")
        });
        assert_eq!(assignment.session_id.as_str(), session_id);
        assert_eq!(assignment.authority.lease.owner, target.owner);
        assert!(assignment.authority.lease.epoch > old_lease.epoch);
        assert!(
            !assignment.environment_creation_not_attempted,
            "{rule}: no fabricated no-Create witness"
        );
        let retained = repo.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            retained.environment, original_environment,
            "{rule}: exact binding retained"
        );
        assert_eq!(
            retained.terminal_cleanup, original_cleanup,
            "{rule}: no preparation/disposal ACK"
        );
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0,
            "{rule}: no ordinary activation"
        );
        assert!(
            runtime.intents.lock().unwrap().is_empty(),
            "{rule}: no physical effect at claim"
        );
    }
}

#[tokio::test]
async fn invalid_worker_demand_projection_does_not_reject_the_session_root() {
    // Causes: C1 a durable root carries a malformed empty realization owner;
    // C2 the capacity index is derived in that root's write transaction.
    // R1=C1+C2 -> the root remains readable for operator recovery, while the
    // operational demand read fails closed instead of publishing zero. The
    // index is not a second admission authority for Session creation.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut root = persisted("invalid-worker-owner", true, "idle");
    root.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: String::new(),
        runtime_incarnation: "invalid:pod".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), root).await;
    assert!(
        repo.get(&SessionId::from("invalid-worker-owner"))
            .await
            .is_ok(),
        "R1 root remains available for repair"
    );
    assert!(
        repo.worker_demand().await.is_err(),
        "R1 no false zero sample"
    );
}

#[tokio::test]
async fn session_worker_demand_counts_live_affinity_and_terminal_cleanup() {
    // Causes: C1 two terminal roots share owner A; C2 one terminal root has B;
    // C3 one unmaterialized terminal root needs first placement; C4 one live,
    // otherwise idle root has C; C5 owner leases expire; C6 B proves cleanup.
    // Effects: R1 C1-C4 -> four Workers (A, B, C, first placement), with the
    // live root present in the indexed scan; R2 +C5+C6 -> three. Expiry alone
    // never lowers demand. This covers the delayed-Dispatch gap without a
    // second Session index or Cloud-local state.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    for (id, owner) in [
        ("terminal-a1", Some("worker-a")),
        ("terminal-a2", Some("worker-a")),
        ("terminal-b", Some("worker-b")),
        ("terminal-unbound", None),
    ] {
        let mut session = persisted(id, false, "terminated");
        session.archive("2026-10-05T00:00:00Z", 1).unwrap();
        if let Some(owner) = owner {
            session.realization = Some(crate::session_resource::SessionRealizationLease {
                owner: owner.into(),
                runtime_incarnation: format!("{owner}:pod-uid"),
                epoch: 1,
                expires_at_unix_ms: 1,
            });
        }
        create(repo.as_ref(), session).await;
    }
    let mut live = persisted("live-c", true, "idle");
    live.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: "worker-c".into(),
        runtime_incarnation: "worker-c:pod-uid".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    });
    create(repo.as_ref(), live).await;
    let indexed = repo
        .scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
        .await
        .unwrap();
    assert!(
        indexed
            .sessions
            .iter()
            .any(|scoped| scoped.session.session_id.as_str() == "live-c"),
        "R1 live affinity is present in the existing recovery index",
    );
    assert_eq!(repo.worker_demand().await.unwrap(), 4, "R1",);
    application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .release_terminal_resources("workspace", "terminal-b")
    .await
    .expect("R2 cleanup")
    .expect("R2 retained Session");
    assert_eq!(repo.worker_demand().await.unwrap(), 3, "R2",);
}

#[tokio::test]
async fn terminal_cleanup_worker_demand_follows_the_verified_session_root() {
    // Causes: C1 execution is live/terminal; C2 cleanup is unrequested,
    // requested, or completed. Effects: R1 live/unrequested -> no terminal Worker demand;
    // R2 terminal/requested/exact -> demand even after its Run settled;
    // R3 completed with an actual root receipt -> no demand. The Session root
    // remains the sole lifecycle owner; forged requested binding is covered by
    // the contract test beside the predicate.
    let live = persisted("demand-live", true, "idle");
    assert!(!live.needs_terminal_cleanup_worker().unwrap(), "R1");
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut terminal = persisted("demand-terminal", false, "terminated");
    terminal.archive("2026-10-05T00:00:00Z", 1).unwrap();
    assert!(terminal.needs_terminal_cleanup_worker().unwrap(), "R2");
    create(repo.as_ref(), terminal).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    application
        .release_terminal_resources("workspace", "demand-terminal")
        .await
        .expect("R3 cleanup")
        .expect("R3 retained Session");
    let completed = repo.get(&SessionId::from("demand-terminal")).await.unwrap();
    assert!(
        completed.terminal_cleanup.is_completed(),
        "R3 durable completion"
    );
    assert!(!completed.needs_terminal_cleanup_worker().unwrap(), "R3");
}

#[tokio::test]
async fn worker_realization_owner_read_keeps_expired_unfinished_session_occupied() {
    // Owner-domain cause/effect table: C1 exact/different Worker, C2 expired
    // realization lease, C3 cleanup incomplete/completed. R1 exact+C2+
    // incomplete -> occupied; R2 different owner -> empty; R3 terminal
    // cleanup completed -> empty. Expiry cannot manufacture physical cleanup.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut retained = persisted("affinity-expired", true, "idle");
    retained.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: "worker-affinity".into(),
        runtime_incarnation: "worker-affinity:pod-uid".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), retained).await;
    let mut terminal = persisted("affinity-completed", false, "terminated");
    terminal.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: "worker-completed".into(),
        runtime_incarnation: "worker-completed:pod-uid".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), terminal).await;
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    assert!(
        application
            .has_unfinished_worker_realizations("worker-affinity")
            .await
            .unwrap(),
        "R1 expired lease remains physical affinity",
    );
    assert!(
        !application
            .has_unfinished_worker_realizations("other-worker")
            .await
            .unwrap(),
        "R2 another Worker owns no matching Session",
    );
    assert!(
        application
            .has_unfinished_worker_realizations("")
            .await
            .is_err(),
        "invalid owner cannot prove an empty domain",
    );
    application
        .release_terminal_resources("workspace", "affinity-completed")
        .await
        .expect("R3 terminal cleanup")
        .expect("R3 retained Session");
    assert!(
        !application
            .has_unfinished_worker_realizations("worker-completed")
            .await
            .unwrap(),
        "R3 completed cleanup releases replacement hold",
    );
}
