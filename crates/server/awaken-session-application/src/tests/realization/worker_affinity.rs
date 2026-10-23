use super::*;

#[tokio::test]
async fn draining_cleanup_claim_only_continues_its_owned_terminal_root() {
    // Causes C1 Ready/Draining; C2 no lease/live exact lease/expired exact lease.
    // Existing roster checks still own protocol, expiry and capabilities.
    // R1 Ready -> ordinary claim/replay. R2 Draining+already-owned root ->
    // preserve recovery, including an expired exact-Actor generation retry.
    // R3 Draining+unbound root -> no acquisition, unchanged complete root.
    // Full Run capacity never blocks finishing an existing cleanup obligation.
    for state in [
        awaken_worker_contract::WorkerState::Ready,
        awaken_worker_contract::WorkerState::Draining,
    ] {
        for ownership in ["unbound", "live", "expired"] {
            let target = registry_race_target();
            let prior = (ownership != "unbound").then(|| {
                crate::session_resource::SessionRealizationLease {
                    owner: target.owner.clone(),
                    runtime_incarnation: target.runtime_incarnation.clone(),
                    epoch: 7,
                    expires_at_unix_ms: if ownership == "live" { u64::MAX } else { 1 },
                }
            });
            let repo = sqlite_session_repository("draining exact cleanup generation").await;
            let id = SessionId::from("draining-exact-generation");
            create(
                repo.as_ref(),
                registry_race_terminal_session(id.as_str(), prior.clone()),
            )
            .await;
            let before = repo.get(&id).await.unwrap();
            let application = application_with_configuration(
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
                SessionApplicationConfiguration {
                    execution_placement: SessionExecutionPlacement::RegisteredWorker,
                    ..Default::default()
                },
            );
            let workers = Arc::new(RecordingWorkerObservations::default());
            let mut worker = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
            worker.snapshot.state = state;
            worker.snapshot.in_flight = worker.snapshot.manifest.capacity.max_concurrent;
            workers.replace(vec![worker]);
            application
                .execution_admission()
                .set_worker_observation_source(workers)
                .unwrap();
            let assignment = application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target, None)
                .await
                .unwrap();
            let admitted =
                state == awaken_worker_contract::WorkerState::Ready || ownership != "unbound";
            assert_eq!(assignment.is_some(), admitted, "{state:?}/{ownership}");
            let after = repo.get(&id).await.unwrap();
            if !admitted || ownership == "live" {
                assert_eq!(after, before, "R2/R3 zero root CAS");
            }
            if ownership == "live" {
                assert_eq!(
                    assignment.unwrap().authority.lease,
                    prior.unwrap(),
                    "R2 exact replay"
                );
            }
        }
    }
}

#[tokio::test]
async fn draining_cleanup_claim_races_preserve_owned_roots_and_compensate_new_ones() {
    // C1 Ready scan; C2 drain before CAS / after CAS / after ambiguous win;
    // C3 predecessor absent/expired exact; C4 concurrent successor on undo.
    // D1-D3 an originally unbound root cannot be acquired on drain, including
    // after an ambiguous CAS. Its exact compensation preserves D4 a concurrent
    // successor. C3 already-owned roots instead continue after drain; their
    // original predecessor, not the just-written lease, establishes affinity.
    // C4 compensation-only fault injection applies to unbound-root rows.
    for row in ["before-cas", "after-cas", "ambiguous", "successor"] {
        for had_predecessor in [false, true] {
            let target = registry_race_target();
            let prior = had_predecessor.then(|| crate::session_resource::SessionRealizationLease {
                owner: target.owner.clone(),
                runtime_incarnation: target.runtime_incarnation.clone(),
                epoch: 7,
                expires_at_unix_ms: 1,
            });
            let inner: Arc<dyn awaken_session_repository_contract::SessionStore> =
                sqlite_session_repository("drain versus root CAS").await;
            let id = SessionId::from("draining-cas-race");
            create(
                inner.as_ref(),
                registry_race_terminal_session(id.as_str(), prior.clone()),
            )
            .await;
            let before = inner.get(&id).await.unwrap();
            let repo = Arc::new(FaultingSessionRepository::new(inner.clone()));
            let successor = crate::session_resource::SessionRealizationLease {
                runtime_incarnation: "worker-registry-race:3:concurrent-successor".into(),
                epoch: 99,
                expires_at_unix_ms: u64::MAX,
                owner: target.owner.clone(),
            };
            if row == "ambiguous" {
                repo.commit_then_conflict_once("claim-terminal-cleanup-recovery");
            } else if row == "successor" {
                repo.commit_concurrent_realization_then_conflict_once(
                    "revoke-terminal-cleanup-recovery",
                    successor.clone(),
                );
            }
            let application = application_with_configuration(
                repo,
                Arc::new(RecordingEnvironmentSource::default()),
                SessionApplicationConfiguration {
                    execution_placement: SessionExecutionPlacement::RegisteredWorker,
                    ..Default::default()
                },
            );
            let workers = Arc::new(RecordingWorkerObservations::default());
            let ready = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
            let mut draining = ready.clone();
            draining.snapshot.state = awaken_worker_contract::WorkerState::Draining;
            workers.replace(vec![draining.clone()]);
            let mut steps = vec![WorkerObservationStep::Workers(vec![ready.clone()])];
            if row != "before-cas" {
                steps.push(WorkerObservationStep::Workers(vec![ready]));
            }
            steps.push(WorkerObservationStep::Workers(vec![draining]));
            workers.script(steps);
            application
                .execution_admission()
                .set_worker_observation_source(workers)
                .unwrap();
            let claimed = application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target, None)
                .await
                .unwrap();
            let after = inner.get(&id).await.unwrap();
            if had_predecessor {
                let claimed = claimed.expect("drain preserves original terminal root recovery");
                assert!(claimed.authority.lease.epoch > prior.as_ref().unwrap().epoch);
                assert_eq!(after.realization, Some(claimed.authority.lease));
            } else {
                assert!(claimed.is_none(), "{row}: no new root on drain");
                assert_eq!(
                    after.realization,
                    if row == "successor" {
                        Some(successor)
                    } else {
                        prior
                    },
                    "exact new-claim compensation {row}"
                );
            }
            assert_eq!(
                after.terminal_cleanup, before.terminal_cleanup,
                "no cleanup ACK"
            );
            assert_eq!(
                after.environment, before.environment,
                "preserve source identity"
            );
            if row == "before-cas" && !had_predecessor {
                assert_eq!(after.revision, before.revision, "D1 no root CAS");
            }
        }
    }
}

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
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
        );
        app.execution_admission()
            .set_worker_observation_source(workers)
            .unwrap();
        let observed = awaken_session_realization_contract::SessionTerminalCleanupQueue::claim_next_terminal_cleanup(app.terminal_cleanups.as_ref(), target.clone(), None).await;
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
    let indexed = repo.reconciliation_scan(None).await.unwrap();
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
    .terminal_cleanups
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
        .terminal_cleanups
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
            .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy("worker-affinity"))
            .await
            .unwrap()
            .occupied,
        "R1 expired lease remains physical affinity",
    );
    assert!(
        !application
            .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy("other-worker"))
            .await
            .unwrap()
            .occupied,
        "R2 another Worker owns no matching Session",
    );
    assert!(
        application
            .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy(""))
            .await
            .is_err(),
        "invalid owner cannot prove an empty domain",
    );
    application
        .terminal_cleanups
        .release_terminal_resources("workspace", "affinity-completed")
        .await
        .expect("R3 terminal cleanup")
        .expect("R3 retained Session");
    assert!(
        !application
            .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy(
                "worker-completed"
            ))
            .await
            .unwrap()
            .occupied,
        "R3 completed cleanup releases replacement hold",
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_requires_current_compatible_worker_before_root_cas() {
    // Worker-admission cause/effect graph: C1 the canonical registry source is
    // absent; C2 the exact current incarnation is registered; C3 its manifest
    // explicitly supports the current runtime protocol; C4 the frozen active Resource
    // generation requires Session Resources and the manifest supplies it; C5
    // an installed Repository has a credential binding and the manifest
    // supplies the Repository-credential capability; C6 an unowned Environment
    // has never been materialized; C7 a retained checkpoint format is
    // explicitly supported; C8 each later scenario starts after the preceding
    // claimed Session's exclusive cursor. E1 only the applicable C2-C7 facts admit
    // the root lease CAS; E2 every missing/stale/incompatible fact leaves the
    // Session root unchanged; E3 Draining may continue an owned live generation,
    // while full Run capacity does not prevent cleanup; E4 physical
    // Environment state without a historical owner fails closed.
    // C9 a matching logical name/checkpoint format does not close an old
    // Runtime or prove physical-domain access. Keep that historical subject
    // pending, and use a separate exact-Runtime expired subject for W8/W9.
    //
    // | Rule | source | protocol | resources | Environment | Effect |
    // |---|---|---|---|---|---|
    // | W1a | absent | - | - | unmaterialized | E2 unavailable |
    // | W1b | read failure | - | - | unmaterialized | E2 unavailable |
    // | W2 | stale incarnation | current | present | unmaterialized | E2 skip |
    // | W3 | Starting | current | present | unmaterialized | E2 skip |
    // | W4 | exact | ANY/zero-based/old/inverted/future | present | unmaterialized | E2 skip, unchanged root |
    // | W5 | exact | current | missing capability | unmaterialized | E2 skip |
    // | W6 | exact Draining/full | current | exact capability | unbound | E2, then Ready claims; Draining replays that exact live assignment |
    // | W7 | exact | current, Repository credential missing/exact | credentialed | E2 then E1 |
    // | W8-old/W9-old | new Runtime | missing/exact checkpoint format | none | Hibernated/old Runtime | pending; zero CAS/effects |
    // | W8 | exact | current, checkpoint format missing | none | Hibernated/exact Runtime | E2 |
    // | W9 | exact | current, checkpoint format exact | none | Hibernated/exact Runtime | E1 |
    // | W10 | exact | current | exact capability | Resident/no owner | E2+E4 |
    let repo = sqlite_session_repository("Worker-compatible cleanup Session repository").await;
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker-compatible".into(),
        runtime_incarnation: "worker-compatible:3:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut requested = registry_race_terminal_session("claim-compatible", None);
    requested.resources = awaken_session_state_contract::SessionResourceState::from_active(
        file_resources("claim-compatible-file"),
    );
    // The original checked factory performs the same request/freeze assertions.
    create(repo.as_ref(), requested).await;

    let observation_runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = configured_application(
        observation_runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
    );
    assert!(
        matches!(
            application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target.clone(), None)
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "W1/E2"
    );
    assert!(
        repo.get(&SessionId::from("claim-compatible"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "W1/E2 zero root mutation"
    );

    let workers = Arc::new(RecordingWorkerObservations::default());
    let mut stale = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    stale.snapshot.identity =
        awaken_worker_contract::WorkerIdentity::new(target.owner.clone(), "stale-boot", 4);
    workers.replace(vec![stale]);
    application
        .execution_admission()
        .set_worker_observation_source(workers.clone())
        .expect("install exact Worker observations");
    workers.fail.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target.clone(), None)
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "W1b/E2"
    );
    workers.fail.store(false, Ordering::SeqCst);
    assert!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .expect("W2 stale Worker is ineligible")
            .is_none(),
        "W2/E2"
    );

    let mut starting = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    starting.snapshot.state = awaken_worker_contract::WorkerState::Starting;
    workers.replace(vec![starting]);
    assert!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .expect("W3 unready Worker is ineligible")
            .is_none(),
        "W3/E2"
    );

    // W4 also includes v3 stage-scoped workers: the whole-claim protocol floor
    // applies equally to ordinary Run admission and terminal cleanup placement.
    for (min, max) in [
        (0, u32::MAX),
        (0, 3),
        (1, 1),
        (1, 2),
        (3, 2),
        (1, 3),
        (3, 3),
        (
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
        ),
    ] {
        let mut manifest = terminal_cleanup_manifest();
        manifest.runtime_protocol.min = min;
        manifest.runtime_protocol.max = max;
        manifest
            .capabilities
            .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
        workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
        let before = repo
            .get(&SessionId::from("claim-compatible"))
            .await
            .unwrap();
        assert!(
            application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target.clone(), None)
                .await
                .expect("W4 protocol-incompatible candidate")
                .is_none(),
            "{min}..{max}"
        );
        assert_eq!(
            repo.get(&SessionId::from("claim-compatible"))
                .await
                .unwrap(),
            before,
            "W4/E2"
        );
    }

    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .expect("W5 missing Resource capability is ineligible")
            .is_none(),
        "W5/E2"
    );

    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    let mut worker = terminal_cleanup_worker(&target, manifest);
    worker.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    worker.snapshot.in_flight = worker.snapshot.manifest.capacity.max_concurrent;
    workers.replace(vec![worker.clone()]);
    assert!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .unwrap()
            .is_none(),
        "W6 draining cannot acquire an unbound root"
    );
    worker.snapshot.state = awaken_worker_contract::WorkerState::Ready;
    workers.replace(vec![worker.clone()]);
    let assignment = application
        .terminal_cleanups
        .claim_next_terminal_cleanup(target.clone(), None)
        .await
        .expect("W6 compatible Worker admission")
        .expect("W6 assignment");
    assert_eq!(assignment.session_id, "claim-compatible", "W6/E1");
    worker.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    workers.replace(vec![worker]);
    assert_eq!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .unwrap()
            .unwrap()
            .authority
            .lease,
        assignment.authority.lease,
        "W6/E3 draining replays only its existing live generation"
    );

    let credentialed_session_id = "claim-d-credential";
    let resources = credentialed_repository_resources("source", "repo-active");
    let mut credentialed = registry_race_terminal_session(credentialed_session_id, None);
    credentialed.resources =
        awaken_session_state_contract::SessionResourceState::from_active(resources);
    create(repo.as_ref(), credentialed).await;
    let mut resource_only_manifest = terminal_cleanup_manifest();
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest.clone(),
    )]);
    assert!(
        application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), Some(&assignment.session_id))
            .await
            .expect("W7 missing Repository credential capability is ineligible")
            .is_none(),
        "W7/E2"
    );
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest,
    )]);
    let credentialed_assignment = application
        .terminal_cleanups
        .claim_next_terminal_cleanup(target.clone(), Some(&assignment.session_id))
        .await
        .expect("W7 credential-compatible Worker admission")
        .expect("W7 assignment");
    assert_eq!(
        credentialed_assignment.session_id, credentialed_session_id,
        "W7/E1"
    );

    let checkpoint_session_id = "claim-e-checkpoint";
    let current_checkpoint_id = "claim-e0-checkpoint-current";
    let mut retained_checkpoint = None;
    let mut checkpoint_assignment = None;
    // One original root factory and one fixture table preserve both separate
    // subjects. Format support never converts the old Runtime into a closed
    // source; only the independent exact-Runtime row reaches W9 continuation.
    for (id, runtime, object_id, exact_runtime) in [
        (
            checkpoint_session_id,
            "worker-compatible:2:old-boot",
            "checkpoint-object",
            false,
        ),
        (
            current_checkpoint_id,
            target.runtime_incarnation.as_str(),
            "checkpoint-current-object",
            true,
        ),
    ] {
        let mut checkpoint = registry_race_terminal_session(
            id,
            Some(crate::session_resource::SessionRealizationLease {
                owner: target.owner.clone(),
                runtime_incarnation: runtime.into(),
                epoch: 5,
                expires_at_unix_ms: 1,
            }),
        );
        checkpoint.environment =
            awaken_session_state_contract::SessionEnvironmentState::Hibernated {
                checkpoint: awaken_session_state_contract::SandboxCheckpointRef {
                    id: object_id.into(),
                    format: "portable-checkpoint-v1".into(),
                    digest: "sha256:checkpoint".into(),
                    size_bytes: 7,
                    created_at_unix_ms: 1,
                    expires_at_unix_ms: u64::MAX,
                    environment_fingerprint: "env-7".into(),
                    base_image_fingerprint: "base-image".into(),
                    excluded_mounts: Vec::new(),
                    suspend_effect_id: "suspend-effect".into(),
                },
                generation: awaken_session_state_contract::SandboxGeneration::new(
                    &SessionId::from(id),
                    1,
                    u64::MAX,
                    "env-7",
                    "base-image",
                ),
            };
        create(repo.as_ref(), checkpoint).await;
        let before = repo.get(&SessionId::from(id)).await.unwrap();
        workers.replace(vec![terminal_cleanup_worker(
            &target,
            terminal_cleanup_manifest(),
        )]);
        assert!(
            application
                .terminal_cleanups
                .claim_next_terminal_cleanup(
                    target.clone(),
                    Some(&credentialed_assignment.session_id),
                )
                .await
                .expect("W8 unsupported checkpoint format is ineligible")
                .is_none(),
            "W8/E2"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "W8 zero root CAS"
        );
        let mut checkpoint_manifest = terminal_cleanup_manifest();
        checkpoint_manifest
            .checkpoint_formats
            .insert("portable-checkpoint-v1".into());
        workers.replace(vec![terminal_cleanup_worker(&target, checkpoint_manifest)]);
        let admitted = application
            .terminal_cleanups
            .claim_next_terminal_cleanup(target.clone(), Some(&credentialed_assignment.session_id))
            .await
            .expect("W9 supported checkpoint admission");
        if exact_runtime {
            let assignment = admitted.expect("W9 exact Runtime assignment");
            assert_eq!(assignment.session_id, current_checkpoint_id, "W9/E1");
            assert_eq!(
                assignment.authority.lease.epoch, 6,
                "W9 exact Runtime epoch5 successor"
            );
            assert_eq!(
                assignment.authority.lease.runtime_incarnation, target.runtime_incarnation,
                "W9 exact Runtime continuation"
            );
            checkpoint_assignment = Some(assignment);
        } else {
            assert!(
                admitted.is_none(),
                "W9-old portability cannot prove closed execution/domain access"
            );
            assert_eq!(
                repo.get(&SessionId::from(id)).await.unwrap(),
                before,
                "W8-old/W9-old original Runtime/checkpoint preserved with zero CAS"
            );
            assert!(
                observation_runtime.effective_ids.lock().unwrap().is_empty()
                    && observation_runtime
                        .publication_intents
                        .lock()
                        .unwrap()
                        .is_empty(),
                "W8-old/W9-old no early preparation, disposal or publication effects"
            );
            retained_checkpoint = Some(before);
        }
    }
    let checkpoint_assignment = checkpoint_assignment.expect("W9 exact Runtime positive row ran");
    let retained_checkpoint = retained_checkpoint.expect("W9-old predecessor negative row ran");

    let mut unowned = registry_race_terminal_session("claim-f-unowned-resident", None);
    unowned.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "physical-without-owner".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    create(repo.as_ref(), unowned).await;
    assert!(
        matches!(
            application
                .terminal_cleanups
                .claim_next_terminal_cleanup(target, Some(&checkpoint_assignment.session_id))
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "W10/E2+E4"
    );
    assert!(
        repo.get(&SessionId::from("claim-f-unowned-resident"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "W10/E2 zero root mutation"
    );
    assert_eq!(
        repo.get(&SessionId::from(checkpoint_session_id))
            .await
            .unwrap(),
        retained_checkpoint,
        "W9-old later exact-Runtime assignment never reassigns the historical checkpoint row",
    );
}
