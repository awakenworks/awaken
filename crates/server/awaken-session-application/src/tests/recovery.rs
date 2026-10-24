//! The Session recovery cycle and its lifecycle supervisor.

use super::realization::sqlite_session_repository;
use super::*;

#[test]
fn lifecycle_cancellation_interrupts_a_held_recovery_scan() {
    run_composed_async_test(|| async {
        // Timing diagnostics add no await, timer, or recovery decision. Causes:
        // cancellation at initial dispatch / candidate scan / final validation
        // => prompt shutdown, no later phase or fabricated successful cycle.
        // These three boundary rows retain the behavioral gate; wall-clock log
        // values are diagnostic data and deliberately have no duration assertion.
        // Cover initial dispatch, recovery candidates and the final audit.
        // No held read may prevent process shutdown; cancellation does not release any
        // physical predecessor or commit a synthetic successful recovery.
        for held_call in [1, 2, 3] {
            let durable: Arc<dyn awaken_session_repository_contract::SessionStore> =
                sqlite_session_repository("session repository").await;
            let observed = Arc::new(FaultingSessionRepository::new(durable));
            observed
                .hold_recovery_scan_on_call
                .store(held_call, Ordering::SeqCst);
            let application = Arc::new(application(
                observed.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            ));
            let cancellation = awaken_runtime_contract::CancellationToken::new();
            let task = tokio::spawn(
                application
                    .clone()
                    .recovery()
                    .run_lifecycle_supervisor(cancellation.clone()),
            );
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                observed.recovery_scan_entered.notified(),
            )
            .await
            .expect("recovery reached the held read");
            cancellation.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("cancellation must not wait for the held read")
                .expect("supervisor did not panic")
                .expect("cancelled cleanly");
            assert_eq!(observed.recovery_scan_count(), held_call);
            assert!(
                application
                    .recovery()
                    .run_lifecycle_supervisor(cancellation)
                    .await
                    .unwrap_err()
                    .contains("already claimed")
            );
        }
    });
}

#[tokio::test(start_paused = true)]
async fn lifecycle_maintenance_is_not_starved_by_a_held_recovery_scan() {
    // Lifecycle-supervisor cause/effect decision table. C1 the initial dispatch
    // scan completes; C2 the following full recovery scan remains blocked past
    // the 30-second maintenance cadence; C3 cancellation arrives while that
    // recovery is still blocked. E1 lease renewal and Work dispatch still enter
    // their one existing maintenance path at the cadence; E2 no second recovery
    // cycle starts concurrently; E3 cancellation drops the owned recovery task
    // and exits. Rules LM1=C1+C2=>E1+E2; LM2=C3=>E3. The repository scan count
    // is the observable dispatch boundary: call 1 is startup dispatch, call 2
    // is the held recovery, and call 3 can only be cadence maintenance.
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> =
        sqlite_session_repository("session repository").await;
    let observed = Arc::new(FaultingSessionRepository::new(durable));
    observed
        .hold_recovery_scan_on_call
        .store(2, Ordering::SeqCst);
    let application = Arc::new(application(
        observed.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let cancellation = awaken_runtime_contract::CancellationToken::new();
    let task = tokio::spawn(
        application
            .recovery()
            .run_lifecycle_supervisor(cancellation.clone()),
    );

    observed.recovery_scan_entered.notified().await;
    assert_eq!(observed.recovery_scan_count(), 2, "LM1 precondition");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..8 {
        if observed.recovery_scan_count() >= 3 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        observed.recovery_scan_count(),
        3,
        "LM1/E1 cadence maintenance must not wait for full recovery"
    );

    cancellation.cancel();
    task.await
        .expect("supervisor did not panic")
        .expect("LM2/E3 cancelled cleanly");
}

/// Recovery-scan causal graph: C1 one supervisor cycle owns Resource,
/// Environment, realization, Event, and Outcome convergence; C2 every handler
/// previously opened the same durable candidate index; C3 cutover validation is
/// a distinct post-cycle audit. Effects are E1 one candidate scan feeds all
/// handlers, E2 handlers reload roots by id before effects, and E3 the final
/// audit remains an independent second scan.
///
/// | Rule | Candidate scan | Cutover audit | Expected scans |
/// |---|---|---|---|
/// | S1 | succeeds | enabled | 2 (E1 + E3) |
/// | S2 | unavailable | not reached | 1 and retryable failure |
#[test]
fn one_recovery_cycle_scans_candidates_once_before_the_final_audit() {
    run_composed_async_test(|| async {
        let durable: Arc<dyn awaken_session_repository_contract::SessionStore> =
            sqlite_session_repository("session repository").await;
        let observed = Arc::new(FaultingSessionRepository::new(durable));
        let application = Arc::new(application(
            observed.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));

        observed.fail_recovery_scan_once();
        let unavailable = application
            .clone()
            .recovery()
            .reconcile_pending_session_state()
            .await;
        assert_eq!(unavailable.retryable_failures, 1, "S2");
        assert_eq!(observed.recovery_scan_count(), 1, "S2");

        let cycle = application
            .recovery()
            .reconcile_pending_session_state()
            .await;
        assert_eq!(cycle.retryable_failures, 0, "S1");
        assert_eq!(observed.recovery_scan_count(), 3, "S1/E1/E3");
    });
}
