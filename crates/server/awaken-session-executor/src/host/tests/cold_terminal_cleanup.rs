//! Remote and cold terminal cleanup assignments and coordinated thread archive.

use super::*;

#[tokio::test]
async fn source_dispatch_observation_preserves_current_scope_and_rejects_affinity_drift() {
    // Source-access cause/effect table: C1 terminal root; C2 frozen scope;
    // C3 runnable member; C4 a parent-affined row outside that scope; C5 a
    // member with a foreign parent. R1=C1+C2+!C3+!C4+!C5 -> Settled;
    // R2=!C1 or !C2 -> unproven; R3=C3 -> Pending; R4=C4|C5 -> invalid
    // identity, even after observing C3. Unrelated rows do not block this root.
    // The reader has only Dispatch read capabilities and changes no row.
    use crate::host::worker_resolver::test_support::test_activation;
    use awaken_dispatch_contract::{
        DispatchEnqueue as _, DispatchReads as _, SessionRunReservations as _,
    };
    use awaken_session_execution_contract::{
        SessionExecutionRecoveryGap as Gap, SessionSourceDispatchObservation as Source,
    };
    for (terminal, frozen, rows, expected) in [
        (true, true, vec![], Source::Settled),
        (
            false,
            true,
            vec![],
            Source::NotProven {
                reason: Gap::NotTerminal,
            },
        ),
        (
            true,
            false,
            vec![],
            Source::NotProven {
                reason: Gap::ScopeNotFrozen,
            },
        ),
        (
            true,
            true,
            vec![("source-root", "source-root")],
            Source::Pending,
        ),
        (
            true,
            true,
            vec![("source-child", "source-root")],
            Source::Pending,
        ),
        (
            true,
            true,
            vec![("source-escaped", "source-root")],
            Source::NotProven {
                reason: Gap::IdentityMismatch,
            },
        ),
        (
            true,
            true,
            vec![("source-child", "foreign-root")],
            Source::NotProven {
                reason: Gap::IdentityMismatch,
            },
        ),
        (
            true,
            true,
            vec![("foreign-child", "foreign-root")],
            Source::Settled,
        ),
        (
            true,
            true,
            vec![
                ("source-root", "source-root"),
                ("source-child", "foreign-root"),
            ],
            Source::NotProven {
                reason: Gap::IdentityMismatch,
            },
        ),
    ] {
        let store = awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
            awaken_lease_testkit::ManualClock::new(100),
        ))
        .await
        .unwrap();
        let projection = remote_terminal_cleanup_projection();
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "source-worker".into(),
            runtime_incarnation: "source-incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: 1_000,
        };
        let mut root = terminal_test_session(
            "source-root",
            &projection,
            lease,
            ["source-child".into()],
            None,
        );
        if terminal {
            root.archive("source-observation", 100).unwrap();
        } else {
            root.execution = awaken_session_state_contract::SessionExecutionState::Idle;
        }
        if !frozen {
            root.terminal_cleanup = Default::default();
            assert!(root.ensure_terminal_cleanup_fence());
        }
        for (index, (thread, parent)) in rows.into_iter().enumerate() {
            // Observe already-admitted activities; do not bypass fresh-root
            // reservation or the canonical child-admission command.
            let request = awaken_dispatch_contract::RunDispatch::new(test_activation(
                thread,
                &format!("source-run-{index}"),
            ))
            .for_session(ThreadId(parent.into()))
            .with_session_activity_epoch(1);
            if thread == parent {
                store.enqueue(request).await.unwrap();
            } else {
                store
                    .enqueue_session_child(
                        request,
                        awaken_dispatch_contract::SessionChildAdmission::new(25, Vec::new()),
                    )
                    .await
                    .unwrap();
            }
        }
        let before = store.list_dispatches().await.unwrap();
        assert_eq!(
            awaken_dispatch_execution::read_source_dispatch_observation(&store, &root).await,
            expected
        );
        assert_eq!(store.list_dispatches().await.unwrap(), before, "read only");
    }
}

#[tokio::test]
async fn ordinary_attempt_acknowledgement_releases_source_without_claiming_global_closure() {
    // Regression O1: an ordinary attempt really returns and its guarded Done
    // removes the current row -> terminal source access is Settled. The same
    // original journal retains only an Acknowledgement, never independent
    // closure. A full-history/independent-closure gate here would regress every
    // normal cleanup. O2 before that return -> Pending, with its slot retained.
    use crate::host::worker_resolver::test_support::test_activation;
    use awaken_dispatch_contract::{
        DispatchAttemptRecordReads as _, DispatchClaims as _, DispatchEnqueue as _,
        DispatchExecution as _, DispatchSettlement as _,
    };
    use awaken_session_execution_contract::SessionSourceDispatchObservation as Source;
    let store = awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
        awaken_lease_testkit::ManualClock::new(100),
    ))
    .await
    .unwrap();
    let projection = remote_terminal_cleanup_projection();
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "source-worker".into(),
        runtime_incarnation: "source-incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: 1_000,
    };
    let mut root = terminal_test_session(
        "source-ordinary",
        &projection,
        lease,
        std::iter::empty(),
        None,
    );
    let run = RunId("source-ordinary-run".into());
    store
        .enqueue(
            awaken_dispatch_contract::RunDispatch::new(test_activation(
                "source-ordinary",
                &run.0,
            ))
            .for_session(ThreadId("source-ordinary".into()))
            .with_session_activity_epoch(1),
        )
        .await
        .unwrap();
    let claimed = store
        .claim_run(&run, "source-worker", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
    assert_eq!(
        store.begin_attempt(&claim, None).await.unwrap(),
        awaken_dispatch_contract::AttemptAdmission::Applied
    );
    root.archive("source-observation", 100).unwrap();
    assert_eq!(
        awaken_dispatch_execution::read_source_dispatch_observation(&store, &root).await,
        Source::Pending,
        "O2"
    );
    assert_eq!(
        store
            .finish_attempt(
                &claim,
                Some(awaken_dispatch_contract::AttemptReturnCause::StructuredReturnObserved)
            )
            .await
            .unwrap(),
        awaken_dispatch_contract::SettleOutcome::Applied
    );
    assert_eq!(
        store
            .settle(
                &run,
                claim.epoch,
                awaken_dispatch_contract::DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap(),
        awaken_dispatch_contract::SettleOutcome::Applied
    );
    assert_eq!(
        awaken_dispatch_execution::read_source_dispatch_observation(&store, &root).await,
        Source::Settled,
        "O1"
    );
    let facts = store
        .attempt_closure_records_after(Default::default(), 10, Some(&run))
        .await
        .unwrap();
    assert_eq!(facts.events.len(), 1);
    assert!(
        matches!(
            &facts.events[0].record,
            awaken_dispatch_contract::AttemptPhysicalFact::Acknowledgement { .. }
        ),
        "O1 remains ordinary return, not independent closure"
    );
}

#[tokio::test]
async fn remote_worker_executes_the_canonical_terminal_cleanup_command_locally() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a Worker-local Session owns a live Sandbox; C2 the
    // global claim-next authority returns its exact durable terminal assignment;
    // C3 the lease is not yet due for ordinary renewal; C4 the aggregate first
    // durably accepts source preparation, then accepts physical disposal but
    // that second transport response is lost; C5 Control reoffers the same
    // terminal assignment on the next recovery scan. Effects: E1 only global
    // assignment recovery enters the aggregate driver; E2 the driver invokes
    // the separate Host preparation and typed physical-disposal ports; E3 the
    // exact preparation and disposal evidence return to Control in order; E4
    // the failed disposal call retains the exact projection; E5 the completed
    // readback retires it without replaying live preparation, physical disposal,
    // or entering renewal/revoke; E6 the failed assignment reports its exact
    // Session without changing the first error classification. No resident scan
    // or second scheduler exists.
    //
    // | Rule | cleanup | renewal due | Effect |
    // | R1 | claimed prepare then disposal/response lost | no | E1 + E2 + E3 + E4 + E6 |
    // | R2 | re-claimed Completed readback | no | E5 |
    // | R3 | fenced empty | any | retain (covered by application table) |
    // R1 receipt observation is scoped and released before the asynchronous R2
    // readback, so the fixture cannot serialize behavior by holding Control's lock.
    use awaken_provisioning_contract::SandboxStatus;

    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let managed = crate::ManagedHost::new(host.clone());
    let mut projection = remote_terminal_cleanup_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        "remote-terminal-worker",
        projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R1 complete projection precedes the live Environment");
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 3,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
    };
    let create_fence = lease
        .sandbox_effect_fence("remote-terminal-create")
        .expect("R1 create fence");
    let physical = host
        .test_local_provider()
        .create_sandbox_for_effect(
            &host
                .realization_executor
                .sandbox_spec("remote-terminal-worker"),
            &create_fence,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create_fence),
        )
        .await
        .expect("R1 exact Worker-local Sandbox");
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(physical)
            .into_session_environment(),
    );
    let binding = serde_json::to_string(&environment.lifecycle().handle())
        .expect("R1 encode live Environment binding");
    let environment_generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("remote-terminal-worker"),
        lease.epoch,
        lease.expires_at_unix_ms,
        "remote-terminal-environment",
        "remote-terminal-image",
    );
    let environment_effect_id = create_fence.operation_id();
    // A changed aggregate Environment is a new root-CAS snapshot.
    projection.revision.0 += 1;
    projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: Some(environment_effect_id.to_owned()),
        generation: Some(environment_generation.clone()),
        idle_since_unix_ms: None,
    };
    let candidate = host
        .realization_executor
        .environments()
        .begin_session_environment_preparation(
            host.realization_executor.peers(),
            "remote-terminal-worker",
            environment.clone(),
            None,
        )
        .expect("R1 retain the exact Worker-local candidate");
    host.install_session_environment_owner_projection(
        "remote-terminal-worker",
        &projection.workspace_id,
        &projection.environment,
    )
    .expect("R1 project the durable Worker-local Environment identity");
    host.realization_executor
        .environments()
        .publish_prepared_session_environment(
            "remote-terminal-worker",
            &candidate,
            crate::environment_owner::BoundSessionEnvironmentIdentity::Durable {
                effect_id: environment_effect_id.to_owned(),
                generation: environment_generation,
            },
        )
        .expect("R1 publish the exact Worker-local Environment owner");
    let assignment = awaken_session_realization_contract::SessionTerminalCleanupAssignment {
        environment_creation_not_attempted: false,
        session_id: "remote-terminal-worker".into(),
        projection: projection.clone(),
        authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
    };
    control
        .assignments
        .lock()
        .unwrap()
        .push_back(assignment.clone());
    let terminal_session = terminal_test_session(
        "remote-terminal-worker",
        &projection,
        lease.clone(),
        std::iter::empty(),
        None,
    );
    let command = terminal_session
        .terminal_cleanup
        .command_for(
            &awaken_session_types::SessionId::from("remote-terminal-worker"),
            "remote-terminal-worker",
        )
        .expect("R1 canonical command");
    *control.session.lock().unwrap() = Some(terminal_session);
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    control
        .disposal_response_loss_once
        .store(true, Ordering::SeqCst);

    let pending = host
        .recover_terminal_cleanup_assignments(target.clone())
        .await
        .expect_err("R1/C4 reports the ambiguous completion response");
    assert_eq!(pending.kind, crate::HostErrorKind::Internal, "R1/E6");
    assert!(
        pending.message.contains("remote-terminal-worker"),
        "R1/E6 identifies the failing assignment"
    );
    assert!(
        host.installed_sessions.contains("remote-terminal-worker"),
        "R1/E4 keeps the exact projection for aggregate readback"
    );
    assert_eq!(
        environment.sandbox_environment().status().await.unwrap(),
        SandboxStatus::Terminated,
        "R1/E2"
    );
    {
        let disposals = control.disposals.lock().unwrap();
        assert_eq!(disposals.len(), 1, "R1/E3");
        assert_eq!(
            disposals[0]
                .provider_disposal
                .prepared_effect_fence()
                .operation_id(),
            command.effect_id,
            "R1/E3",
        );
    }
    control.assignments.lock().unwrap().push_back(assignment);
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target.clone())
            .await
            .expect("R2 Completed readback"),
        1,
        "R2/E5 reconciles the completed terminal generation without renewal"
    );
    assert!(
        !host.installed_sessions.contains("remote-terminal-worker"),
        "R2/E5 retires the exact local tree"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target.clone(), target.clone(), target],
        "R1-R2 each recovery claims one assignment then observes the empty queue"
    );
    drop(managed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_terminal_cleanup_renews_its_generation_and_another_session() {
    // Cause/effect graph: C1 global claim-next returns one cold terminal
    // assignment and its canonical driver blocks after aggregate authorization;
    // C2 that terminal generation and another ordinary Session owned by the
    // Worker are both due for renewal; C3 the terminal authorization is later
    // released. Effects: E1 C1 uniquely installs and retains the terminal slot;
    // E2 both Sessions reach the sole aggregate lease-renewal port and install
    // monotonic same-generation readback before C3; E3 no resident scan, second
    // cleanup driver, or local work queue is introduced; E4 the old asserted
    // terminal effect continues under its renewed current generation and reaches
    // its durable boundary. Constraint: the Session root remains the only
    // renewal/terminal decision owner; renewal does not wait for long-lived
    // terminal I/O.
    //
    // | Rule | cleanup blocked | terminal due | ordinary due | release | Effect |
    // |---|---|---|---|---|---|
    // | R1 | yes | yes | yes | no | E1 + E2 + E3 |
    // | R2 | yes | renewed | renewed | yes | E4 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let managed = crate::ManagedHost::new(host.clone());
    let now_unix_ms = awaken_session_realization::test_support::wall_clock_now_ms();
    let expiring_unix_ms = now_unix_ms.saturating_add(5_000);
    // Renewal extends the asserted Control coordinate by one renewal interval;
    // the Worker never supplies its own wall-clock epoch.
    let requested_expiry_unix_ms = expiring_unix_ms + 10_000;

    let ordinary_id = "renewal-beside-terminal";
    let ordinary_projection = remote_terminal_cleanup_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        ordinary_id,
        ordinary_projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R1/C2 install ordinary frozen projection");
    let ordinary_lease = crate::session_resource::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 8,
        expires_at_unix_ms: expiring_unix_ms,
    };
    host.install_session_realization_lease(ordinary_id, ordinary_lease.clone())
        .unwrap();
    control
        .renewals
        .lock()
        .unwrap()
        .insert(ordinary_id.into(), ordinary_lease.clone());

    let terminal_id = "blocked-terminal-cleanup";
    let mut terminal_projection = remote_terminal_cleanup_projection();
    let terminal_lease = crate::session_resource::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 9,
        expires_at_unix_ms: expiring_unix_ms,
    };
    fresh_terminal_fixture_before_create(terminal_id, &mut terminal_projection, &terminal_lease);
    control.assignments.lock().unwrap().push_back(
        awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            environment_creation_not_attempted: false,
            session_id: terminal_id.into(),
            projection: terminal_projection.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(
                terminal_lease.clone(),
            ),
        },
    );
    *control.session.lock().unwrap() = Some(terminal_test_session(
        terminal_id,
        &terminal_projection,
        terminal_lease.clone(),
        std::iter::empty(),
        None,
    ));
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: terminal_lease.owner.clone(),
        runtime_incarnation: terminal_lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: terminal_lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    let cleanup_started = Arc::new(tokio::sync::Notify::new());
    let cleanup_release = Arc::new(tokio::sync::Notify::new());
    *control.authorization_barrier.lock().unwrap() =
        Some((cleanup_started.clone(), cleanup_release.clone()));

    let cleanup = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        cleanup_started.notified(),
    )
    .await
    .expect("R1/C1 terminal driver reaches the blocking authorization");

    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            host.renew_due_session_realizations(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000,),
            ),
        )
        .await
        .expect("R1/E2 renewal cadence is independent from terminal I/O")
        .expect("R1/E2 aggregate renewal succeeds"),
        2,
        "R1/E2"
    );
    assert!(!cleanup.is_finished(), "R1/E1");
    let mut renewed_sessions = control.renewal_sessions.lock().unwrap().clone();
    renewed_sessions.sort();
    assert_eq!(
        renewed_sessions,
        [terminal_id.to_owned(), ordinary_id.to_owned()],
        "R1/E2 both Sessions use the sole lease-only renewal port"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(ordinary_id, |r| {
                r.execution()
                    .authority
                    .lease()
                    .map(|lease| lease.expires_at_unix_ms)
            })
            .expect("installed Sessions")
            .flatten(),
        Some(requested_expiry_unix_ms),
        "R1/E2"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(terminal_id, |r| {
                r.execution()
                    .authority
                    .lease()
                    .map(|lease| lease.expires_at_unix_ms)
            })
            .expect("installed Sessions")
            .flatten(),
        Some(requested_expiry_unix_ms),
        "R1/E2 terminal slot installs authoritative renewal readback"
    );

    cleanup_release.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), cleanup)
            .await
            .expect("R2/E4 cleanup reaches its durable boundary")
            .expect("R2/E4 cleanup task joins")
            .expect("R2/E4 cleanup succeeds"),
        1,
        "R2/E4 one claimed terminal assignment completes"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "R1-R2 one exact claim plus the terminating empty scan"
    );
    drop(managed);
}

fn enqueue_completed_terminal_assignments(
    control: &RemoteTerminalCleanupControl,
    lease: &crate::session_resource::SessionRealizationLease,
    prefix: &str,
    count: usize,
) {
    for index in 0..count {
        // Recovery's exclusive Session cursor is lexical, so fixture IDs
        // preserve their intended ascending order across 9 -> 10.
        let session_id = format!("{prefix}-{index:03}");
        control.assignments.lock().unwrap().push_back(
            awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                environment_creation_not_attempted: false,
                session_id: session_id.clone().into(),
                projection: remote_terminal_cleanup_projection(),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    lease.clone(),
                ),
            },
        );
        control
            .renewals
            .lock()
            .unwrap()
            .insert(session_id, lease.clone());
    }
}

fn completed_terminal_test_lease() -> crate::session_resource::SessionRealizationLease {
    crate::session_resource::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 3,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
    }
}

fn terminal_target_for(
    lease: &crate::session_resource::SessionRealizationLease,
) -> awaken_session_realization_contract::SessionRealizationTarget {
    awaken_session_realization_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_terminal_cleanup_replenishes_a_slot_while_one_sibling_is_blocked() {
    // Cause/effect graph: C1 five distinct terminal assignments are offered by
    // the sole Control queue; C2 the first canonical work poll blocks; C3 one
    // sibling poll fails; C4 the first is released. E1 the other three polls
    // proceed while C2 is blocked; E2 a freed slot claims and completes the
    // fifth assignment before C4; E3 the failure does not cancel sibling work;
    // E4 the sweep reports the error after C4; E5 the report names the failed
    // sibling even though another Session was blocked and the rest succeeded.
    // Constraint: each assignment has its own Session lease; the mock returns
    // completed work so existing tests remain the authority for physical effects.
    //
    // | Rule | offered | first poll | sibling failure | Effect |
    // | R1 | five | blocked | yes | E1 + E2 + E3 |
    // | R2 | five | released | yes | E4 + E5 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let lease = completed_terminal_test_lease();
    let target = terminal_target_for(&lease);
    enqueue_completed_terminal_assignments(&control, &lease, "bounded-terminal", 5);
    *control.terminal_work_block_for.lock().unwrap() = Some("bounded-terminal-000".into());
    *control.terminal_work_fail_for.lock().unwrap() = Some("bounded-terminal-002".into());
    let recovery = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        control.terminal_work_started.notified(),
    )
    .await
    .expect("R1/C2 first canonical poll blocks");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if control.terminal_work_seen.lock().unwrap().len() == 5 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("R1/E2 fifth Session proceeds despite blocked first");
    assert!(control.claim_targets.lock().unwrap().len() >= 5, "R1/E2");
    assert!(!recovery.is_finished(), "R1/C2");

    control.terminal_work_release.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), recovery)
        .await
        .expect("R2/E4 sweep completes after release")
        .expect("R2/E4 recovery task joins")
        .expect_err("R2/E4 sibling failure remains visible");
    assert!(error.to_string().contains("injected terminal work failure"));
    assert!(error.to_string().contains("bounded-terminal-002"), "R2/E5");
    assert_eq!(control.terminal_work_seen.lock().unwrap().len(), 5, "R2/E3");
    assert_eq!(control.claim_targets.lock().unwrap().len(), 6, "R2/E4");
    assert!(control.assignments.lock().unwrap().is_empty(), "R2/E4");
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target)
            .await
            .unwrap(),
        0,
        "R2/E4 completed assignments are not replayed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_terminal_cleanup_never_drives_more_than_four_assignments() {
    // Causes: C1 five independent assignments exist; C2 four polls block at
    // their canonical Control port; C3 one poll is released. Effects: E1 no
    // fifth claim occurs while four are active; E2 C3 admits the fifth while
    // the other three remain blocked; E3 active calls never exceed four and
    // every claimed assignment completes. Rules B1=C1+C2->E1,
    // B2=B1+C3->E2+E3. The gate changes only test I/O timing, not lease truth.
    // Constraint: the fixture publishes poll arrival and active/max counters
    // under the same arrival lock. Observing the fifth arrival must imply its
    // active increment; observing four active calls must wait for their arrivals
    // through that lock. Neither assertion may sample between those events.
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let lease = completed_terminal_test_lease();
    let target = terminal_target_for(&lease);
    enqueue_completed_terminal_assignments(&control, &lease, "bounded-active", 5);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *control.terminal_work_gate.lock().unwrap() = Some(gate.clone());
    let recovery =
        tokio::spawn(async move { host.recover_terminal_cleanup_assignments(target).await });

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if control.terminal_work_active.load(Ordering::SeqCst) == 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B1 four calls reached the test gate");
    assert_eq!(control.claim_targets.lock().unwrap().len(), 4, "B1/E1");
    assert_eq!(control.terminal_work_seen.lock().unwrap().len(), 4, "B1/E1");

    gate.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if control.terminal_work_seen.lock().unwrap().len() == 5 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B2/E2 fifth call starts after one finishes");
    assert_eq!(
        control.terminal_work_active.load(Ordering::SeqCst),
        4,
        "B2/E3"
    );
    assert_eq!(
        control.max_terminal_work_active.load(Ordering::SeqCst),
        4,
        "B2/E3"
    );

    gate.add_permits(4);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), recovery)
            .await
            .expect("B2/E3 recovery completes")
            .expect("B2/E3 recovery task joins")
            .expect("B2/E3 every call succeeds"),
        5,
        "B2/E3"
    );
}

#[tokio::test]
async fn cold_terminal_claim_failure_drains_claimed_work_and_retries_the_remainder() {
    // Causes: C1 three independent assignments exist; C2 claim call three
    // fails after two leases have been returned; C3 the next sweep retries.
    // Effects: E1 both claimed Sessions finish through the canonical poll;
    // E2 the claim error remains visible; E3 the unclaimed third assignment
    // finishes once on C3, with no replay of E1. Rules P1=C1+C2->E1+E2 and
    // P2=P1+C3->E3. A failed claim never removes an assignment from Control.
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let lease = completed_terminal_test_lease();
    let target = terminal_target_for(&lease);
    enqueue_completed_terminal_assignments(&control, &lease, "partial-claim", 3);
    *control.claim_failure_on_call.lock().unwrap() = Some(3);

    let error = host
        .recover_terminal_cleanup_assignments(target.clone())
        .await
        .expect_err("P1/E2 claim failure is reported");
    assert!(
        error
            .to_string()
            .contains("injected terminal claim failure")
    );
    assert_eq!(control.terminal_work_seen.lock().unwrap().len(), 2, "P1/E1");
    assert_eq!(control.assignments.lock().unwrap().len(), 1, "P1/E3");

    *control.claim_failure_on_call.lock().unwrap() = None;
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target)
            .await
            .unwrap(),
        1,
        "P2/E3 only the remaining assignment is claimed"
    );
    let seen = control.terminal_work_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "P2/E3");
    assert_eq!(
        seen.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    assert!(control.assignments.lock().unwrap().is_empty(), "P2/E3");
}

#[tokio::test]
async fn cold_terminal_recovery_preserves_the_sixty_four_assignment_sweep_cap() {
    // Causes: C1 sixty-five independently completed assignments are offered;
    // C2 one bounded sweep ends; C3 a later sweep runs. Effects: E1 C2 claims
    // exactly sixty-four with a strictly advancing request-local cursor; E2 C3
    // resets the cursor, claims the final assignment, and reaches the empty
    // queue. R1=C1+C2->E1, R2=R1+C3->E2. The cursor grants no ownership.
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let lease = completed_terminal_test_lease();
    let target = terminal_target_for(&lease);
    enqueue_completed_terminal_assignments(&control, &lease, "sweep-cap", 65);
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target.clone())
            .await
            .unwrap(),
        64,
        "R1/E1"
    );
    assert_eq!(control.assignments.lock().unwrap().len(), 1, "R1/E1");
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target)
            .await
            .unwrap(),
        1,
        "R2/E2"
    );
    assert_eq!(control.terminal_work_seen.lock().unwrap().len(), 65);
    assert!(control.assignments.lock().unwrap().is_empty(), "R2/E2");
    let cursors = control.claim_after.lock().unwrap();
    assert_eq!(
        cursors.first(),
        Some(&None),
        "R1 first claim starts at the root"
    );
    assert_eq!(
        cursors.get(1),
        Some(&Some("sweep-cap-000".into())),
        "R1 second claim advances past the prior assignment"
    );
    assert_eq!(cursors.get(64), Some(&None), "R2 starts a new sweep");
    assert_eq!(
        cursors.get(65),
        Some(&Some("sweep-cap-064".into())),
        "R2 empty scan advances past its one claimed Session"
    );
}

#[tokio::test]
async fn cold_terminal_recovery_rejects_a_nonadvancing_legacy_cursor_reply() {
    // C1 a new Worker sends an exclusive cursor to an old Control that ignores
    // it; C2 the same terminal assignment is returned again. R1 C1+C2 ->
    // report protocol nonprogress, drain the first in-flight cleanup once,
    // and never execute a second physical effect under the duplicate lease.
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let lease = completed_terminal_test_lease();
    enqueue_completed_terminal_assignments(&control, &lease, "legacy-cursor", 1);
    let duplicate = control.assignments.lock().unwrap().front().unwrap().clone();
    control.assignments.lock().unwrap().push_back(duplicate);

    let error = host
        .recover_terminal_cleanup_assignments(terminal_target_for(&lease))
        .await
        .expect_err("R1 old Control must not cause duplicate cleanup");
    assert!(error.to_string().contains("cursor did not advance"), "R1");
    assert_eq!(control.terminal_work_seen.lock().unwrap().len(), 1, "R1");
    assert_eq!(
        control.claim_after.lock().unwrap().as_slice(),
        &[None, Some("legacy-cursor-000".into())],
        "R1 old Control ignored the second cursor"
    );
}

#[tokio::test]
async fn cold_terminal_assignment_installs_then_uses_the_canonical_cleanup_path() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Session has no process-local slot after
    // Worker replacement; C2 claim-next returns a typed routing assignment with
    // the frozen projection, an active Repository manifest, and a newly fenced
    // lease but no Run claim; C3 the first Control poll classifies the terminal
    // fence; C4 the canonical driver re-reads the same aggregate work; C5 the
    // closed preparation authorization succeeds. Effects: E1 the driver is the
    // sole installer of the exact baseline and lease before C5, without
    // re-materializing the Resource being destroyed; E2 it durably records
    // source preparation before projecting typed physical disposal; E3 the
    // exact disposal receipt returns through Control and removes the slot only
    // after aggregate completion; E4 claim-next then returns empty and the
    // bounded recovery scan stops without a Worker-local queue or duplicate
    // cleanup path. Constraint: a claim never installs a projection or carries
    // commands; the driver installs current readback before source I/O.
    //
    // | Rule | local slot | Control boundary | Effect |
    // |---|---|---|---|
    // | C1 | absent | before claim | remains absent |
    // | C2 | installed | authorization admitted | E1 before source I/O |
    // | C3 | installed | root prepare then dispose | E2 + E3 |
    // | C4 | removed | next claim is empty | E4 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let authorization_started = Arc::new(tokio::sync::Notify::new());
    let authorization_proceed = Arc::new(tokio::sync::Notify::new());
    *control.authorization_barrier.lock().unwrap() =
        Some((authorization_started.clone(), authorization_proceed.clone()));
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let managed = crate::ManagedHost::new(host.clone());
    let session_id = "cold-terminal-worker";
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:replacement".into(),
        epoch: 4,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
    };
    let mut projection = remote_terminal_cleanup_projection();
    projection.resource_revision = 1;
    projection.resources = effective_repository(
        "terminal-cleanup-resource",
        "/must-not-be-cloned-during-terminal-cleanup",
        "/workspace/cleanup",
        None,
    );
    fresh_terminal_fixture_before_create(session_id, &mut projection, &lease);
    control.assignments.lock().unwrap().push_back(
        awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            environment_creation_not_attempted: false,
            session_id: session_id.into(),
            projection: projection.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
        },
    );
    let terminal_session = terminal_test_session(
        session_id,
        &projection,
        lease.clone(),
        std::iter::empty(),
        None,
    );
    let command = terminal_session
        .terminal_cleanup
        .command_for(
            &awaken_session_types::SessionId::from(session_id),
            session_id,
        )
        .expect("C3 canonical root command");
    *control.session.lock().unwrap() = Some(terminal_session);
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    assert!(!host.installed_sessions.contains(session_id), "C1");

    let recovery = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        authorization_started.notified(),
    )
    .await
    .expect("C2 canonical driver reaches authorization after its unique install");
    assert_eq!(
        host.installed_sessions
            .realizations()
            .frozen(session_id)
            .expect("installed Sessions")
            .baseline
            .clone()
            .expect("C1/E1 frozen baseline")
            .fingerprint,
        remote_terminal_cleanup_projection().baseline.fingerprint,
        "C1/E1"
    );
    assert!(
        host.installed_sessions
            .realizations()
            .read_realization(session_id, |r| r.staging().resources.mounts.is_empty())
            .expect("installed Sessions")
            .unwrap_or(false),
        "C1/E1 terminal installation must not materialize active Resources"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(session_id, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions")
            .flatten(),
        Some(lease.clone()),
        "C1/E1"
    );
    authorization_proceed.notify_one();
    assert_eq!(
        recovery.await.unwrap().expect("C2-C4 cold recovery"),
        1,
        "C2/E2"
    );
    assert!(!host.installed_sessions.contains(session_id), "C2/E2/E3");
    let disposals = control.disposals.lock().unwrap();
    assert_eq!(disposals.len(), 1, "C2/E3");
    assert_eq!(
        disposals[0]
            .provider_disposal
            .prepared_effect_fence()
            .operation_id(),
        command.effect_id,
        "C2/E3",
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "C1-C4/E4"
    );
    drop(disposals);
    drop(managed);
}

#[tokio::test]
async fn coordinated_thread_archive_uses_disposition_and_dispatch_as_one_recoverable_saga() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 child disposition is Active/Archived; C2 its queue
    // state is absent, Pending, or Awaiting; C3 archive/admission ordering is
    // ordinary or raced. Effects: E1 idle archive commits the one Thread
    // disposition and exact retry is a no-op; E2 active Pending is rejected before
    // disposition mutation; E3 one Awaiting-Thread archive call records ordinary
    // cancellation, waits for the Worker-owned terminal settlement, then commits
    // Archived; E4 an exact archive replay also waits for a stale raced admission
    // to settle without another relationship/archive store or client retry loop.
    //
    // | Rule | Disposition | Dispatch | Ordering | Effect |
    // |---|---|---|---|---|
    // | A1 | Active | absent/idle | archive | E1 |
    // | A2 | Active | Pending | archive | E2 |
    // | A3 | Active | Awaiting | archive | E3 |
    // | A4 | Archived | Pending | stale admission | E4 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, RemoteInputReason, ResumeTicket};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{DispatchOutcome, DispatchState, RunDispatch};

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("archive saga dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_coordinator_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let parent = ThreadId("archive-saga-parent".into());
    let commit = host
        .thread_reads
        .commit_for_read(&parent.0)
        .await
        .expect("parent partition");

    let idle = ThreadId("archive-saga-idle".into());
    let idle_run = RunId("archive-saga-idle-run".into());
    commit
        .commit(ThreadCommit::assemble(
            idle.clone(),
            RunDisposition::ended(idle_run.clone(), EndCause::NaturalEnd),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A1 idle child truth");
    host.archive_session_thread(&parent.0, &idle)
        .await
        .expect("A1 archive");
    host.archive_session_thread(&parent.0, &idle)
        .await
        .expect("A1 exact retry");
    assert_eq!(
        host.thread_reads
            .session_thread_disposition(&parent.0, &idle)
            .await
            .expect("A1 disposition"),
        awaken_agent_contract::ThreadDisposition::Archived,
        "A1/E1"
    );

    let pending = ThreadId("archive-saga-pending".into());
    let pending_run = RunId("archive-saga-pending-run".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &pending.0,
                &pending_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(11),
        )
        .await
        .expect("A2 pending admission");
    let rejected = host
        .archive_session_thread(&parent.0, &pending)
        .await
        .expect_err("A2 running work cannot archive");
    assert_eq!(rejected.kind, HostErrorKind::BadRequest, "A2/E2");
    assert_eq!(
        host.thread_reads
            .session_thread_disposition(&parent.0, &pending)
            .await
            .expect("A2 disposition"),
        awaken_agent_contract::ThreadDisposition::Active,
        "A2/E2"
    );
    dispatch
        .cancel(&pending_run)
        .await
        .expect("A2 cleanup intent");
    clock.set(0);
    let pending_cancel = dispatch
        .claim("archive-test", 100, &Default::default())
        .await
        .expect("A2 cancellation claim")
        .expect("A2 cancellation work");
    dispatch
        .settle(
            &pending_run,
            pending_cancel.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A2 cleanup settle");

    let awaiting = ThreadId("archive-saga-awaiting".into());
    let awaiting_run = RunId("archive-saga-awaiting-run".into());
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::running(awaiting_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 running child truth");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &awaiting.0,
                &awaiting_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(12),
        )
        .await
        .expect("A3 child admission");
    clock.set(0);
    let awaiting_claim = dispatch
        .claim("archive-test", 100, &Default::default())
        .await
        .expect("A3 claim")
        .expect("A3 work");
    dispatch
        .settle(
            &awaiting_run,
            awaiting_claim.lease.epoch(),
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("A3 await");
    let ticket = ResumeTicket::new(
        "archive-saga-correlation",
        awaiting_run.clone(),
        awaiting.clone(),
        "archive-saga-snapshot",
        "archive-saga-catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "archive-saga-call".into(),
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::awaiting(ticket),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 Awaiting child truth");
    let archive_awaiting = {
        let host = host.clone();
        let parent = parent.clone();
        let awaiting = awaiting.clone();
        tokio::spawn(async move { host.archive_session_thread(&parent.0, &awaiting).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let rows = dispatch.list_dispatches().await.expect("A3 cancellation");
            if rows.iter().any(|row| {
                row.run_id == awaiting_run
                    && matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
                    && row.cancellation_requested
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A3 archive records ordinary cancellation before waiting");
    clock.set(0);
    let awaiting_cancel = dispatch
        .claim("archive-test", 100, &Default::default())
        .await
        .expect("A3 cancellation claim")
        .expect("A3 cancellation work");
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::ended(awaiting_run.clone(), EndCause::NaturalEnd),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 terminal interruption truth");
    dispatch
        .settle(
            &awaiting_run,
            awaiting_cancel.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A3 terminal settle");
    archive_awaiting
        .await
        .expect("A3 archive task")
        .expect("A3/E3 one call archives after terminal settle");
    assert_eq!(
        host.thread_reads
            .session_thread_disposition(&parent.0, &awaiting)
            .await
            .expect("A3 disposition"),
        awaken_agent_contract::ThreadDisposition::Archived,
        "A3/E3"
    );

    let raced_run = RunId("archive-saga-raced-run".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &idle.0,
                &raced_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(13),
        )
        .await
        .expect("A4 stale admission after archive");
    let archive_raced = {
        let host = host.clone();
        let parent = parent.clone();
        let idle = idle.clone();
        tokio::spawn(async move { host.archive_session_thread(&parent.0, &idle).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let rows = dispatch.list_dispatches().await.expect("A4 cancellation");
            if rows
                .iter()
                .any(|row| row.run_id == raced_run && row.cancellation_requested)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A4 archived replay records cancellation before waiting");
    clock.set(0);
    let raced_cancel = dispatch
        .claim("archive-test", 100, &Default::default())
        .await
        .expect("A4 cancellation claim")
        .expect("A4 cancellation work");
    assert!(raced_cancel.cancellation_requested, "A4/E4");
    dispatch
        .settle(
            &raced_run,
            raced_cancel.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A4 cancellation settle");
    archive_raced
        .await
        .expect("A4 archive task")
        .expect("A4/E4 one replay converges after raced work settles");
}

use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchClaims as _, DispatchEnqueue as _, DispatchReads as _,
    DispatchSettlement as _,
};
