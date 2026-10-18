// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal evidence session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
//! Session realization reconciliation, renewal, and control deadlines.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_session_reconciliation_is_earliest_deadline_first_and_bounded() {
    use std::sync::atomic::Ordering;

    /* Large reconciliation cause/effect graph: C1 sixty-four independent
     * resident Sessions are simultaneously due at the same Control renewal port;
     * C2 every renewal blocks at one deterministic gate; C3 the Worker-wide bound
     * is eight; C4 lease deadlines differ; C5 the gate is released. Effects:
     * E1 exactly eight renewals enter before release; E2 no ninth enters; E3
     * the first wave contains the eight earliest deadlines; E4 all sixty-four
     * eventually renew; E5 each Session is called exactly once. Constraint:
     * the per-Session realization lock and aggregate Fence remain the only
     * effect authority; this scheduler owns capacity and ordering only.
     *
     * | Rule | Sessions | blocked | cap | Effect |
     * |---|---:|---|---:|---|
     * | L1 | 64 | yes | 8 | E1 + E2 + E3 |
     * | L2 | 64 | released | 8 | E4 + E5 |
     */
    const SESSION_COUNT: usize = 64;
    let authority_now = awaken_session_realization::test_support::wall_clock_now_ms();
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *control.renewal_gate.lock().unwrap() = Some(gate.clone());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    for index in 0..SESSION_COUNT {
        let session_id = format!("mass-session-{index:03}");
        install_test_renewal_authority(
            &host,
            &control,
            &session_id,
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: authority_now + 5_000 + index as u64 * 10,
            },
        );
    }

    let running_host = host.clone();
    let reconciliation = tokio::spawn(async move {
        running_host
            .run_assembly
            .renew_due_session_realizations(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(15_000),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if control.renewal_sessions.lock().unwrap().len()
                == crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("L1 first bounded wave enters");
    assert_eq!(
        control.renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "L1/E1"
    );
    assert_eq!(
        control.max_renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "L1/E2"
    );
    let first_wave = control
        .renewal_sessions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        first_wave,
        (0..crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS)
            .map(|index| format!("mass-session-{index:03}"))
            .collect::<Vec<_>>(),
        "L1/E3"
    );

    gate.add_permits(SESSION_COUNT);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), reconciliation)
            .await
            .expect("L2 bounded waves finish")
            .expect("L2 reconciliation task")
            .expect("L2 reconciliation result"),
        SESSION_COUNT,
        "L2 every due lease renewed"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "L2/E4"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "L2/E4");
    let mut observed = control
        .renewal_sessions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    observed.sort();
    observed.dedup();
    assert_eq!(observed.len(), SESSION_COUNT, "L2/E5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_twelve_session_reconciliation_remains_bounded_and_complete() {
    use std::sync::atomic::Ordering;

    /* Scale cause/effect graph: C1 five hundred twelve resident Sessions have
     * exact live leases outside the renewal proof window; C2 repeated sweeps run
     * before any lease is due; C3 one later sweep makes every lease due; C4 the
     * production renewal cap is eight; C5 an immediate post-renewal sweep is
     * again non-due. Effects: E1 C2 emits no Control renewal, terminal poll, or
     * cold claim and retains every local projection; E2 C3 renews every Session
     * exactly once; E3 concurrency remains bounded; E4 C5 emits no additional
     * Control traffic. This is a deterministic scheduler test, not a production
     * latency claim; live HTTP/PostgreSQL latency remains a deployment
     * measurement.
     *
     * | Rule | local monotonic observation | renewal due | Sessions | Effect |
     * |---|---:|---|---:|---|
     * | S1 | three immediate sweeps | no | 512 | E1 |
     * | S2 | 11s elapsed from a 20s proof | yes | 512 | E2 + E3 |
     * | S3 | immediately after renewal | no after S2 | 512 | E4 |
     */
    const SESSION_COUNT: usize = 512;
    let authority_now = awaken_session_realization::test_support::wall_clock_now_ms();
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    for index in 0..SESSION_COUNT {
        let session_id = format!("scale-session-{index:04}");
        install_test_renewal_authority(
            &host,
            &control,
            &session_id,
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: authority_now + 30_000 + index as u64,
            },
        );
    }
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000);
    for _ in 0..3 {
        assert_eq!(
            host.run_assembly
                .renew_due_session_realizations(timing)
                .await
                .expect("S1 repeated non-due scale sweep succeeds"),
            0,
            "S1/E1"
        );
    }
    assert!(
        control.renewal_sessions.lock().unwrap().is_empty(),
        "S1/E1 no renewal Control traffic"
    );
    assert!(control.events.lock().unwrap().is_empty(), "S1/E1 no poll");
    assert!(
        control.claim_targets.lock().unwrap().is_empty(),
        "S1/E1 no cold claim"
    );
    for index in 0..SESSION_COUNT {
        assert!(
            host.run_assembly
                .installed_sessions
                .contains(&format!("scale-session-{index:04}")),
            "S1/E1 retains every local projection"
        );
    }

    // Exercise actual proof-clock progression; a synthetic Worker wall-clock
    // argument no longer selects due state. This leaves about 9s of proof for
    // the bounded 3s batch and does not alter any durable fence locally.
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.run_assembly.renew_due_session_realizations(timing),
        )
        .await
        .expect("S2/E3 scale scan remains bounded")
        .expect("S2 scale renewal succeeds"),
        SESSION_COUNT,
        "S2/E2 every due lease renews"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "S2/E2"
    );
    assert!(
        control.max_renewals_active.load(Ordering::SeqCst)
            <= crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "S2/E3"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "S2/E3");

    assert_eq!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .expect("S3 post-renewal non-due sweep succeeds"),
        0,
        "S3/E4"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "S3/E4 no additional renewal Control traffic"
    );
    assert!(
        control.events.lock().unwrap().is_empty(),
        "S3/E4 no terminal poll"
    );
    assert!(
        control.claim_targets.lock().unwrap().is_empty(),
        "S3/E4 no cold claim"
    );
}

/// Renewal-response decision table: C1 Control I/O is pending; C2 the selected
/// slot/lease is current, replaced, or removed; C3 reply is success, temporary
/// failure, or conclusive ownership loss. R1 replaced/removed x every reply ->
/// no overwrite, interrupt, revocation, or slot recreation. R2 current + success
/// -> monotonic lease update without waiting for Stage (existing heartbeat test).
/// R3 current + temporary failure -> preserve live authority (existing timeout
/// matrix). R4 current + ownership loss -> wait for realization admission, then
/// recheck the exact tuple before revocation. Claim installation has its own
/// two-case admission test; this test drives real asynchronous Control replies.
/// A 5s receipt has at most 3.334s of proof; a 30s cadence gives a 10s
/// due threshold, so every row enters Control without a rounding-boundary race.
#[tokio::test]
async fn late_renewal_response_cannot_mutate_a_replacement_projection() {
    use awaken_session_realization_contract::SessionRealizationControlFailure as Failure;
    for failure in [
        None,
        Some(Failure::Unavailable("offline".into())),
        Some(Failure::StaleOwnership),
    ] {
        for changed in [
            "owner",
            "incarnation",
            "epoch",
            "expiry",
            "removed",
            "replaced_slot",
        ] {
            let control = Arc::new(RemoteTerminalCleanupControl::default());
            *control.renewal_failure.lock().unwrap() = failure.clone();
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *control.renewal_gate.lock().unwrap() = Some(gate.clone());
            let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_session_realization_control(control.clone())
                .with_authority(
                    crate::UpstreamAuthority::default()
                        .with_terminal_cleanup_control(control.clone()),
                ))
            .build()
            .expect("validated Host dependencies");
            let now = awaken_session_realization::test_support::wall_clock_now_ms();
            let old = crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "boot-1".into(),
                epoch: 1,
                expires_at_unix_ms: now + 5_000,
            };
            install_test_renewal_authority(&host, &control, "renew-race", old.clone());
            let fence = host
                .run_assembly
                .installed_sessions
                .realizations()
                .realization_lock("renew-race")
                .expect("installed Sessions");
            let guard = fence.lock().await;
            let sweep = {
                let host = host.clone();
                tokio::spawn(async move {
                    host.run_assembly
                        .renew_due_session_realizations(
                            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
                                30_000,
                            ),
                        )
                        .await
                })
            };
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while control.renewal_sessions.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("R2 renewal enters Control despite held Stage fence");
            let mut replacement = old;
            match changed {
                "owner" => replacement.owner = "replacement".into(),
                "incarnation" => replacement.runtime_incarnation = "boot-2".into(),
                "epoch" => replacement.epoch += 1,
                "expiry" => replacement.expires_at_unix_ms += 1,
                "removed" | "replaced_slot" => {}
                _ => unreachable!(),
            }
            let claim = awaken_dispatch_contract::RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("new-run".into()),
                owner: "replacement".into(),
                epoch: 2,
            };
            if changed == "removed" {
                host.run_assembly.installed_sessions.remove("renew-race");
            } else {
                if changed == "replaced_slot" {
                    host.run_assembly.installed_sessions.remove("renew-race");
                }
                host.run_assembly
                    .install_session_realization_lease("renew-race", replacement.clone())
                    .unwrap();
                host.run_assembly
                    .installed_sessions
                    .update("renew-race", |slot| {
                        slot.run.dispatch_claim = Some(claim.clone());
                        slot.replace_frozen_with(|next| {
                            next.workspace = Some("new-workspace".into())
                        });
                    });
            }
            gate.add_permits(1);
            drop(guard);
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(1), sweep)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                0,
                "R1 {failure:?}/{changed}"
            );
            if changed == "removed" {
                assert!(!host.run_assembly.installed_sessions.contains("renew-race"));
            } else {
                assert_eq!(
                    host.run_assembly
                        .installed_sessions
                        .read("renew-race", |slot| (
                            slot.realization.execution().authority.lease().cloned(),
                            slot.run.dispatch_claim.clone(),
                            slot.inputs.workspace.clone()
                        )),
                    Some((Some(replacement), Some(claim), Some("new-workspace".into()))),
                    "R1 {failure:?}/{changed}"
                );
            }
        }
    }
}

#[tokio::test]
async fn renewal_revocation_rechecks_authority_after_realization_admission() {
    // R4a conclusive loss while Stage holds admission -> no destructive effects
    // until Stage releases. R4b Stage installs a new lease while revocation waits
    // -> recheck absorbs the obsolete response and preserves the new projection.
    // The 30s cadence makes the 5s receipt unambiguously due, as in R1 above.
    for replaced in [false, true] {
        let control = Arc::new(RemoteTerminalCleanupControl::default());
        *control.renewal_failure.lock().unwrap() = Some(
            awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
        );
        let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_session_realization_control(control.clone())
            .with_authority(
                crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
            ))
        .build()
        .expect("validated Host dependencies");
        let now = awaken_session_realization::test_support::wall_clock_now_ms();
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "boot".into(),
            epoch: 1,
            expires_at_unix_ms: now + 5_000,
        };
        install_test_renewal_authority(&host, &control, "renew-revoke", lease.clone());
        let fence = host
            .run_assembly
            .installed_sessions
            .realizations()
            .realization_lock("renew-revoke")
            .expect("installed Sessions");
        let guard = fence.lock().await;
        let sweep = {
            let host = host.clone();
            tokio::spawn(async move {
                host.run_assembly
                    .renew_due_session_realizations(
                        awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while control.renewal_sessions.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::task::yield_now().await;
        assert!(
            !sweep.is_finished(),
            "R4 revocation waits for current admission"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .contains("renew-revoke")
        );
        let mut next = lease;
        next.epoch += 1;
        if replaced {
            host.run_assembly
                .install_session_realization_lease("renew-revoke", next.clone())
                .unwrap();
        }
        drop(guard);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), sweep)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization("renew-revoke", |r| r.execution().authority.lease().cloned())
                .expect("installed Sessions")
                .flatten(),
            replaced.then_some(next),
            "R4 exact replacement protection"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .contains("renew-revoke"),
            replaced
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temporary_control_failure_retains_only_a_still_live_session_lease() {
    /* Renewal failure cause/effect table: C1 Control does not answer before its
     * authority-derived request deadline; C2 the prior durable lease is either
     * still live or already expired. Effects: E1 a live projection remains installed for the next
     * sweep and execution is not spuriously cancelled; E2 an expired
     * projection is interrupted and revoked. Constraint: no local grace period
     * extends the durable expiry.
     *
     * | Rule | Control | prior lease | Effect |
     * |---|---|---|---|
     * | F1 | timeout/Unavailable | live | E1 + diagnostic error |
     * | F2 | timeout/Unavailable | expired | E2 |
     * Both outcomes must finish within the test deadline: revocation owns the
     * lifecycle lock, so its interruption read must not re-enter that lock.
     * Tokio transport deadlines are paused, independently from the platform
     * proof clock. F1 has 90s of real authority and is due under the 300s
     * cadence. F2 explicitly installs already-exhausted observed evidence.
     * No synthetic Worker wall-clock or short real lease selects these rows.
     */
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let now = awaken_session_realization::test_support::wall_clock_now_ms();
    for (session_id, expired) in [("renewal-live", false), ("renewal-expired", true)] {
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a:incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: now + 90_000,
        };
        let proof = if expired {
            awaken_session_realization::test_support::test_expired_observed_lease(lease)
        } else {
            awaken_session_realization::test_support::test_observed_lease(lease)
        };
        host.run_assembly
            .installed_sessions
            .install_realization(session_id, |r| {
                r.execution_mut().authority =
                    crate::realization_state::SessionRealizationAuthority::Live(proof);
            });
    }

    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        host.run_assembly.renew_due_session_realizations(
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(300_000),
        ),
    )
    .await
    .expect("F2 revocation cannot deadlock its interruption read")
    .expect_err("F1 keeps the retry diagnostic visible");
    assert!(
        host.run_assembly
            .installed_sessions
            .contains("renewal-live"),
        "F1/E1"
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .contains("renewal-expired"),
        "F2/E2"
    );
}

#[tokio::test]
async fn stale_ownership_revokes_even_when_the_old_deadline_is_still_live() {
    /* Explicit-loss rule F3: a live timestamp is necessary but not sufficient
     * after Control reports StaleOwnership. The newer durable owner wins, so
     * the old Worker must interrupt and remove its local projection in the same
     * reconciliation; it may not consume the remaining timestamp as grace.
     * Completion is bounded even when no resident Runtime supplies a reader:
     * the cold interruption read must reuse the held lifecycle guard.
     */
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_failure.lock().unwrap() =
        Some(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control),
        ))
    .build()
    .expect("validated Host dependencies");
    let now = awaken_session_realization::test_support::wall_clock_now_ms();
    host.run_assembly
        .install_session_realization_lease(
            "renewal-stale",
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:old".into(),
                epoch: 1,
                expires_at_unix_ms: now.saturating_add(5_000),
            },
        )
        .unwrap();

    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            host.run_assembly.renew_due_session_realizations(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(15_000),
            )
        )
        .await
        .expect("F3 cold interruption cannot re-enter the lifecycle lock")
        .expect("F3 explicit loss is a handled terminal outcome"),
        0,
        "F3"
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .contains("renewal-stale"),
        "F3"
    );
}

#[tokio::test(start_paused = true)]
async fn per_request_deadlines_release_every_renewal_capacity_slot() {
    use std::sync::atomic::Ordering;

    /* Deadline/capacity cause-effect table: C1 sixteen due lease renewals never
     * answer; C2 the renewal cap is eight; C3 the
     * authority timing yields a 50s virtual per-request deadline; C4 all old leases
     * remain live. Effects: E1 the first eight enter, time out, and release
     * their slots; E2 the queued eight exhaust the SAME budget before entry; E3 no cancelled future leaks
     * an active slot; E4 all local projections remain for retry.
     *
     * | Rule | renewals | response | deadline/cap | Effect |
     * |---|---:|---|---|---|
     * | D1 | 16 | never | 50s virtual / 8 | E1 + E2 + E3 + E4 |
     * Tokio auto-advances one timeout wave; the real 90s proof horizon
     * remains live. The test does not equate scheduling time and OS elapsed time.
     */
    const SESSION_COUNT: usize = 16;
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_realization_control(control.clone())
        .with_authority(
            crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
        ))
    .build()
    .expect("validated Host dependencies");
    let now = awaken_session_realization::test_support::wall_clock_now_ms();
    for index in 0..SESSION_COUNT {
        host.run_assembly
            .install_session_realization_lease(
                &format!("deadline-session-{index:02}"),
                crate::session_resource::SessionRealizationLease {
                    owner: "worker-a".into(),
                    runtime_incarnation: "worker-a:incarnation".into(),
                    epoch: 1,
                    expires_at_unix_ms: now.saturating_add(90_000),
                },
            )
            .unwrap();
    }

    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        host.run_assembly.renew_due_session_realizations(
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(300_000),
        ),
    )
    .await
    .expect("D1 one deadline includes both capacity waiting and Control IO")
    .expect_err("D1 each live Session retains a retry diagnostic");
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "D1/E1-E2"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "D1/E3");
    assert_eq!(
        control.max_renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "D1/E1"
    );
    for index in 0..SESSION_COUNT {
        assert!(
            host.run_assembly
                .installed_sessions
                .contains(&format!("deadline-session-{index:02}")),
            "D1/E4"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn control_deadline_releases_capacity_before_expired_session_mutex_cleanup() {
    use std::sync::atomic::Ordering;

    /* Deadline/mutex composition table: C1 sixteen due Control calls never
     * answer; C2 the first eight expired Sessions have their canonical
     * realization mutex held by an earlier phase; C3 the Control capacity is
     * eight; C4 one request budget elapses, including queue residence. Effects: E1 the first wave releases
     * all Control permits at its deadline even though revocation must wait; E2
     * queued requests expire before entering Control, without weakening or bypassing any held Session
     * mutex; E3 concurrency never exceeds eight; E4 releasing the original
     * guards lets exact revocation finish and leaves no local authority.
     *
     * | Rule | Control | first-wave mutex | Effect |
     * |---|---|---|---|
     * | M1 | first eight time out | held | E1; cleanup waits |
     * | M2 | queued eight expire before entry | first still held | E2 + E3 |
     * | M3 | all timed out | released | E4 |
     * Tokio semaphore/mutex wakeups and virtual-time scheduling are the test
     * platform boundary; the production typed permit kernels own admission,
     * identity recheck, bounded capacity, and terminal release decisions.
     */
    // M4 cancellation while first-wave completions hold scheduler permits and
    // wait for exact mutexes also drops queued-request timeout completions. No
    // physical cleanup may consume a held Session. Synchronous admission close
    // is required before waiting; cancellation keeps Closed and its exact lease.
    // Existing revocation selection (not Live-only renewal) owns cleanup retry.
    let mut observations = Vec::new();
    for cancel_waiting_completion in [false, true] {
        const SESSION_COUNT: usize = 16;
        let control = Arc::new(RemoteTerminalCleanupControl::default());
        *control.renewal_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_session_realization_control(control.clone())
            .with_authority(
                crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
            ))
        .build()
        .expect("validated Host dependencies");
        let now = awaken_session_realization::test_support::wall_clock_now_ms();
        let mut held = Vec::new();
        for index in 0..SESSION_COUNT {
            let session = format!("deadline-mutex-{index:02}");
            let lease = crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: now.saturating_add(90_000),
            };
            host.run_assembly
                .installed_sessions
                .install_realization(&session, |r| {
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Live(
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                lease.clone(),
                            ),
                        );
                });
            control
                .renewals
                .lock()
                .unwrap()
                .insert(session.clone(), lease);
            if index < crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS {
                held.push(
                    host.run_assembly
                        .installed_sessions
                        .realizations()
                        .realization_lock(&session)
                        .expect("installed Sessions")
                        .lock_owned()
                        .await,
                );
            }
        }

        // Capture exactly the eight original mutex allocations and complete
        // retained lease bytes before the actual sweep can wait on cleanup.
        let held_frame = || {
            (0..crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS)
                .map(|index| {
                    let session = format!("deadline-mutex-{index:02}");
                    host.run_assembly.installed_sessions.realizations().read_realization(&session, |r| serde_json::json!({
                    "session": session,
                    "identity": format!("{:p}", Arc::as_ptr(&r.execution().phase)),
                    "lease": serde_json::to_string(&match &r.execution().authority {
                        crate::realization_state::SessionRealizationAuthority::Live(proof) => Some(&proof.lease),
                        crate::realization_state::SessionRealizationAuthority::Closed(lease) => lease.as_ref(),
                        crate::realization_state::SessionRealizationAuthority::Uninstalled => None,
                    }).unwrap(),
                    "closed": r.execution().authority.is_closed(),
                    "locked": r.execution().phase.try_lock().is_err(),
                })).expect("installed Sessions").unwrap_or(serde_json::Value::Null)
                })
                .collect::<Vec<_>>()
        };
        let before_wait = held_frame();
        let running_host = host.clone();
        let reconciliation = tokio::spawn(async move {
            running_host
                .run_assembly
                .renew_due_session_realizations(
                    awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(300_000),
                )
                .await
        });
        // Yield past the one 50s request deadline without a runnable spin loop
        // that would prevent Tokio's paused clock from advancing.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(
            control.renewal_sessions.lock().unwrap().len(),
            crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
            "M1-M2 queued calls never receive another deadline wave"
        );
        assert_eq!(
            control.max_renewals_active.load(Ordering::SeqCst),
            crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
            "M2/E3"
        );
        assert!(
            !reconciliation.is_finished(),
            "M1/E2 held per-Session mutex is never bypassed"
        );
        for index in 0..crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS {
            assert!(
                host.run_assembly
                    .installed_sessions
                    .contains(&format!("deadline-mutex-{index:02}")),
                "M1/E2 held Session remains installed"
            );
        }

        observations.push(serde_json::json!({
            "rule": if cancel_waiting_completion { "cancel-wait" } else { "finish-wait" },
            "observation": {"kind":"mutex", "before":before_wait,
                "after":held_frame(), "pending":!reconciliation.is_finished(),
                "entered":control.renewal_sessions.lock().unwrap().len(),
                "peak":control.max_renewals_active.load(Ordering::SeqCst), "capacity":8}
        }));
        if cancel_waiting_completion {
            let before: Vec<_> = (0..SESSION_COUNT)
                .map(|index| {
                    host.run_assembly
                        .installed_sessions
                        .realizations()
                        .read_realization(&format!("deadline-mutex-{index:02}"), |r| {
                            (r.execution().phase.clone(), r.execution().authority.clone())
                        })
                        .expect("installed Sessions")
                        .unwrap()
                })
                .collect();
            reconciliation.abort();
            assert!(
                reconciliation.await.unwrap_err().is_cancelled(),
                "M4 cancellation is joined"
            );
            assert_eq!(
                control.renewals_active.load(Ordering::SeqCst),
                0,
                "M4 cancelled transport futures are gone"
            );
            for (index, (identity, lease)) in before.iter().enumerate() {
                host.run_assembly
                    .installed_sessions
                    .read(&format!("deadline-mutex-{index:02}"), |slot| {
                        assert!(
                            Arc::ptr_eq(&slot.realization.execution().phase, identity),
                            "M4 exact slot is retained"
                        );
                        assert_eq!(
                            &slot.realization.execution().authority,
                            lease,
                            "M4 cancellation cannot revoke a waiting projection"
                        );
                    })
                    .expect("M4 no queued cleanup removes its Session");
            }
            observations.push(serde_json::json!({"rule":"cancel-joined",
                "observation":{"kind":"mutex", "before":before_wait,
                    "after":held_frame(), "pending":false,
                    "entered":control.renewal_sessions.lock().unwrap().len(),
                    "peak":control.max_renewals_active.load(Ordering::SeqCst), "capacity":8}}));
            drop(held);
            assert_eq!(
                host.run_assembly
                    .revoke_all_session_realizations()
                    .await
                    .unwrap(),
                SESSION_COUNT,
                "M4 existing revocation selection resumes all closed/live cleanup"
            );
        } else {
            drop(held);
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(2), reconciliation)
                    .await
                    .expect("M3 cleanup finishes after original mutex release")
                    .expect("M3 reconciliation task")
                    .expect("M3 expired timeout is a handled revocation"),
                0,
                "M3/E4"
            );
        }
        assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "M3/E4");
        for index in 0..SESSION_COUNT {
            assert!(
                !host
                    .run_assembly
                    .installed_sessions
                    .contains(&format!("deadline-mutex-{index:02}")),
                "M3/E4"
            );
        }
    }
    awaken_reliability_testkit::write_observation_trace(
        "realization-capacity-mutex",
        "realization-capacity",
        serde_json::json!(observations),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_realization_renewal_cannot_replace_or_revoke_new_projection() {
    // Cause/effect graph: C1 a due renewal has captured lease A and blocks at
    // Control; C2 canonical projection installation replaces A with generation
    // B, extends A past the requested expiry, or removes the slot; C3 the old
    // request returns success, explicit ownership loss, or Unavailable after
    // A's expiry. Effects: E1 retain the newer projection and its dispatch claim;
    // E2 never resurrect a removed slot; E3 retain existing fail-closed behavior
    // when no projection changed (covered by F1-F3 above).
    //
    // | Rule | old response | projection during I/O | Effect |
    // | P1 | success | new generation B | E1 |
    // | P2 | success | same generation, later expiry | E1 |
    // | P3 | success | removed | E2 |
    // | P4 | StaleOwnership | new generation B | E1 |
    // | P5 | StaleOwnership | same generation, later expiry | E1 |
    // | P6 | Unavailable, A expired | newer live generation B | E1 |
    // | P7 | StaleOwnership | B installed with owner lock held during completion | E1 |
    // | P8 | StaleOwnership | removed | E2 |
    // The barrier exercises the production renewal completion and revocation
    // paths, not a duplicate lease reducer. Preserving B does not authorize a
    // model request: the exact dispatch/Work checks remain mandatory there.
    use awaken_session_realization_contract::SessionRealizationControlFailure as Failure;
    let mut violations = Vec::new();
    for (rule, failure, replacement, expired) in [
        ("P1", None, "generation", false),
        ("P2", None, "extension", false),
        ("P3", None, "removed", false),
        ("P4", Some(Failure::StaleOwnership), "generation", false),
        ("P5", Some(Failure::StaleOwnership), "extension", false),
        (
            "P6",
            Some(Failure::Unavailable("delayed transport".into())),
            "generation",
            true,
        ),
        ("P7", Some(Failure::StaleOwnership), "generation", false),
        ("P8", Some(Failure::StaleOwnership), "removed", false),
    ] {
        let control = Arc::new(RemoteTerminalCleanupControl::default());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *control.renewal_gate.lock().unwrap() = Some(gate.clone());
        *control.renewal_failure.lock().unwrap() = failure;
        let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_session_realization_control(control.clone())
            .with_authority(
                crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
            ))
        .build()
        .expect("validated Host dependencies");
        let now = awaken_session_realization::test_support::wall_clock_now_ms();
        let old = crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a:old".into(),
            epoch: 1,
            expires_at_unix_ms: if expired {
                now.saturating_sub(1)
            } else {
                now + 1_000
            },
        };
        if expired {
            host.run_assembly
                .installed_sessions
                .install_realization(rule, |r| {
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Live(
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                old.clone(),
                            ),
                        );
                });
            control
                .renewals
                .lock()
                .unwrap()
                .insert(rule.into(), old.clone());
        } else {
            install_test_renewal_authority(&host, &control, rule, old.clone());
        }
        let task_host = host.clone();
        let renewal = tokio::spawn(async move {
            task_host
                .run_assembly
                .renew_due_session_realizations(
                    awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(60_000),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while control.renewal_sessions.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("C1 old request reached the controlled barrier");
        let mut newer = old;
        newer.expires_at_unix_ms = now + 120_000;
        if replacement == "generation" {
            newer.runtime_incarnation = "worker-a:new".into();
            newer.epoch = 2;
        }
        let claim = awaken_dispatch_contract::RunClaim {
            run_id: RunId("new-attempt".into()),
            owner: "worker-a:new".into(),
            epoch: 2,
        };
        let realization = host
            .run_assembly
            .installed_sessions
            .realizations()
            .realization_lock(rule)
            .expect("installed Sessions");
        let installation = if rule == "P7" {
            Some(realization.lock().await)
        } else {
            None
        };
        if installation.is_some() {
            gate.add_permits(1);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while control.renewals_active.load(Ordering::SeqCst) != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("P7 response completes during owner-locked installation");
        }
        if replacement == "removed" {
            host.run_assembly.installed_sessions.remove(rule);
        } else {
            host.run_assembly
                .install_session_realization_lease(rule, newer.clone())
                .unwrap();
            host.run_assembly
                .installed_sessions
                .update_run(rule, |run| run.dispatch_claim = Some(claim.clone()));
        }
        if installation.is_none() {
            gate.add_permits(1);
        }
        drop(installation);
        let _outcome = tokio::time::timeout(std::time::Duration::from_secs(2), renewal)
            .await
            .expect("old completion does not hang")
            .expect("renewal task did not panic");
        let observed = host.run_assembly.installed_sessions.read(rule, |slot| {
            (
                slot.realization.execution().authority.lease().cloned(),
                slot.run.dispatch_claim.clone(),
            )
        });
        let preserved = if replacement == "removed" {
            observed.is_none()
        } else {
            observed == Some((Some(newer), Some(claim)))
        };
        if !preserved {
            violations.push(rule);
        }
    }
    assert!(
        violations.is_empty(),
        "delayed renewal violated projection ownership: {violations:?}"
    );
}
