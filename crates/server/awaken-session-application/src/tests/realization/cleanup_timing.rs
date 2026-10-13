//! Cold cleanup timing exercised through the real Registry and Session root.

use super::*;
use awaken_lease_testkit::ManualClock;
use awaken_worker_contract::WorkerCurrentSource;
use awaken_worker_contract::{
    RegistryMutation, WorkerDirectory, WorkerHeartbeat, WorkerRegistration,
};
use awaken_worker_registry::SqliteWorkerDirectory;

#[tokio::test]
async fn live_session_usage_projects_the_same_clock_that_opened_its_root_interval() {
    // Causes: U1 a typed resident root opens one interval from the installed
    // Clock; U2 that clock advances 0/7999/8000ms or regresses; U3 Runtime has
    // no token usage and the already-observed child set is empty. Effects:
    // exact root active seconds 0/7/8/0, never ambient wall time or an underflow;
    // all reads leave the durable root unchanged. This qualifies accounting
    // over a declared typed residency fixture, not actual provider residency.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let clock = Arc::new(ManualClock::new(1_000_000));
    let id = SessionId::from("clock-usage");
    let mut session = persisted(id.as_str(), false, "idle");
    session.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "typed-usage-fixture".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let mut session = SessionAggregate::try_from(session).expect("valid admission fixture");
    SessionApplication::open_activity_on_snapshot(
        &mut session,
        None,
        false,
        false,
        clock.now_ms(),
        0,
        None,
    )
    .unwrap();
    create(repo.as_ref(), session.into_snapshot()).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .with_clock(clock.clone());
    let before = repo.get(&id).await.unwrap();
    for (now, seconds) in [(1_000_000, 0), (1_007_999, 7), (1_008_000, 8), (999_999, 0)] {
        clock.set(now);
        let usage = app.session_usage(id.as_str(), Some(&[])).await.unwrap();
        assert_eq!(usage.active_seconds, seconds, "U1/U2 {now}");
        assert_eq!(
            repo.get(&id).await.unwrap(),
            before,
            "U3 accounting read is a stutter"
        );
    }
}

#[tokio::test]
async fn cold_cleanup_registry_window_and_receipt_use_one_application_clock_after_reopen() {
    // Cause/effect graph: C1 the configured Coordinator clock is authoritative;
    // C2 a real persisted Worker heartbeat has TTL/age; C3 the requested cleanup
    // horizon fits its remaining window; C4 the Registry is already expired.
    // R1/R2/R4 C1+C2+C3+!C4 => one root CAS, exact receipt horizon=expiry-C1;
    // R3 !C3 and R5 C4 => no assignment and the complete root is unchanged.
    // R6 a live exact-incarnation Session with a 20s lease and a 15s cold
    // proposal reuses its current lease immediately with zero root CAS; an
    // exclusive cursor then skips it. R7 reopening both SQLite owners retains
    // the same heartbeat/root/receipt coordinates. The Worker TTL calculator's
    // existing rows supply these
    // contract examples; this case owns actual root admission, not another TTL
    // calculator or a fake Registry. No physical cleanup is claimed by this test.
    for (rule, ttl, age, claim, current_lease_ms, admitted) in [
        ("R1", 30_000, 10_001, 15_000, None, true),
        ("R2", 6_000, 2_001, 3_000, None, true),
        ("R3", 30_000, 20_000, 15_000, None, false),
        ("R4", 60_000, 20_001, 20_000, None, true),
        ("R5", 30_000, 30_000, 15_000, None, false),
        ("R6", 30_000, 10_000, 15_000, Some(20_000), true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let workers_path = directory.path().join("workers.sqlite");
        let sessions_path = directory.path().join("sessions.sqlite");
        let sessions_path = sessions_path.to_str().unwrap();
        let workers = SqliteWorkerDirectory::open(&workers_path).await.unwrap();
        let clock = Arc::new(ManualClock::new(1_000_000));
        let now = clock.now_ms();
        let heartbeat_at = now - age;
        let mut manifest = terminal_cleanup_manifest();
        manifest
            .capabilities
            .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.into());
        let registered = workers
            .register(
                WorkerRegistration {
                    worker_id: "clock-cleanup-worker".into(),
                    incarnation_id: "boot".into(),
                    manifest,
                    create_ledger: None,
                },
                heartbeat_at,
                ttl,
            )
            .await
            .unwrap();
        assert_eq!(
            workers
                .heartbeat(
                    &registered.snapshot.identity,
                    WorkerHeartbeat {
                        sequence: 1,
                        ready: true,
                        in_flight: 0,
                        warm_environment_shapes: Default::default(),
                        credential_observations: Default::default(),
                        acp_capability_observations: Default::default(),
                    },
                    heartbeat_at,
                    ttl
                )
                .await
                .unwrap(),
            RegistryMutation::Applied
        );
        drop(workers);
        let workers = Arc::new(SqliteWorkerDirectory::open(&workers_path).await.unwrap());
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(sessions_path)
                .await
                .unwrap(),
        );
        let id = SessionId::from("clock-cleanup");
        let mut requested = persisted(id.as_str(), true, "terminated");
        requested.resources = awaken_session_state_contract::SessionResourceState::from_active(
            file_resources("clock-cleanup-file"),
        );
        assert!(requested.terminal_cleanup.request(&id));
        requested
            .terminal_cleanup
            .freeze_targets(&id, [], 0, 0)
            .unwrap();
        if let Some(current_lease_ms) = current_lease_ms {
            requested.realization = Some(crate::session_resource::SessionRealizationLease {
                owner: registered.snapshot.identity.worker_id.clone(),
                runtime_incarnation: registered.snapshot.identity.lease_owner(),
                epoch: 7,
                expires_at_unix_ms: now + current_lease_ms,
            });
        }
        create(repo.as_ref(), requested).await;
        let before = repo.get(&id).await.unwrap();
        let app = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
        )
        .with_clock(clock.clone());
        app.set_worker_observation_source(workers.clone()).unwrap();
        let target = awaken_session_realization_contract::SessionRealizationTarget {
            owner: registered.snapshot.identity.worker_id.clone(),
            runtime_incarnation: registered.snapshot.identity.lease_owner(),
            lease_expires_at_unix_ms: now + claim,
            reassign_existing_lease: false,
        };
        let assigned = app
            .claim_next_terminal_cleanup(target.clone(), None)
            .await
            .unwrap();
        assert_eq!(assigned.is_some(), admitted, "{rule} actual root gate");
        let mut after = repo.get(&id).await.unwrap();
        if let Some(assigned) = assigned {
            if let Some(current_lease_ms) = current_lease_ms {
                assert_eq!(assigned.authority.lease.epoch, 7, "R6 current epoch");
                assert_eq!(
                    assigned.authority.lease.expires_at_unix_ms,
                    now + current_lease_ms,
                    "R6 short cold proposal does not replace the live lease"
                );
                assert_eq!(after.revision, before.revision, "R6 zero root CAS");
                assert!(
                    app.claim_next_terminal_cleanup(target, Some(&id))
                        .await
                        .unwrap()
                        .is_none(),
                    "R6 cursor prevents a second claim in the same sweep"
                );
                continue;
            }
            assert_eq!(
                assigned.authority.remaining_ms.get(),
                claim,
                "{rule} same Clock receipt"
            );
            assert_eq!(
                assigned.authority.lease.expires_at_unix_ms, target.lease_expires_at_unix_ms,
                "{rule} exact admitted horizon"
            );
            assert_eq!(after.realization.as_ref(), Some(&assigned.authority.lease));
            assert!(
                after.revision > before.revision,
                "{rule} root CAS committed"
            );
            // R7 time advances during the same generation => renewal commits
            // one larger expiry and publishes an authority-relative horizon.
            // R9 the original assertion later expires while that renewal is
            // live => exact replay returns the current 99ms horizon, no CAS.
            // R8 the current lease itself reaches expiry => refusal, whole
            // root unchanged even when a proposed future expiry is valid.
            let original = assigned.authority.lease.clone();
            clock.advance(100);
            let renewed = app
                .renew_session_realization(
                    awaken_session_realization_contract::RenewSessionRealization {
                        session_id: id.clone(),
                        asserted_lease: original.clone(),
                        requested_expires_at_unix_ms: original.expires_at_unix_ms + 100,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                renewed.remaining_ms.get(),
                claim,
                "R7/{rule} current horizon"
            );
            assert_eq!(renewed.lease.owner, original.owner, "R7 exact owner");
            assert_eq!(
                renewed.lease.runtime_incarnation, original.runtime_incarnation,
                "R7 exact incarnation"
            );
            assert_eq!(renewed.lease.epoch, original.epoch, "R7 exact generation");
            after = repo.get(&id).await.unwrap();
            clock.set(original.expires_at_unix_ms + 1);
            let replay = app
                .renew_session_realization(
                    awaken_session_realization_contract::RenewSessionRealization {
                        session_id: id.clone(),
                        asserted_lease: original,
                        requested_expires_at_unix_ms: renewed.lease.expires_at_unix_ms,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                replay.lease, renewed.lease,
                "R9/{rule} original identity, current receipt"
            );
            assert_eq!(
                replay.remaining_ms.get(),
                99,
                "R9/{rule} current Clock horizon"
            );
            assert_eq!(
                repo.get(&id).await.unwrap(),
                after,
                "R9/{rule} replay is a stutter"
            );
            clock.set(renewed.lease.expires_at_unix_ms);
            assert!(matches!(app.renew_session_realization(
                awaken_session_realization_contract::RenewSessionRealization {
                    session_id: id.clone(), asserted_lease: renewed.lease.clone(),
                    requested_expires_at_unix_ms: renewed.lease.expires_at_unix_ms + 100,
                },
            ).await, Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)), "R8/{rule}");
            assert_eq!(
                repo.get(&id).await.unwrap(),
                after,
                "R8/{rule} expired refusal preserves the whole root"
            );
        } else {
            assert_eq!(
                after, before,
                "{rule} refusal leaves the whole root unchanged"
            );
        }
        drop(app);
        drop(repo);
        drop(workers);
        let reopened = awaken_session_store::SqliteManagedSessionRepository::open(sessions_path)
            .await
            .unwrap();
        assert_eq!(
            reopened.get(&id).await.unwrap(),
            after,
            "R6/{rule} root survives reopen"
        );
        let reopened_workers = SqliteWorkerDirectory::open(&workers_path).await.unwrap();
        let current = reopened_workers
            .current(&target.owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            current.heartbeat_at_ms, heartbeat_at,
            "R6/{rule} exact heartbeat"
        );
        assert_eq!(
            current.snapshot.expires_at_ms,
            heartbeat_at + ttl,
            "R6/{rule} exact expiry"
        );
    }
}
