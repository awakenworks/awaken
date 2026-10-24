// @formal evidence run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
//! The pool spends the dispatch retry budget on failed claim resolutions.
use super::*;

/// Failed-resolution budget decision table (ADR-0015 D1 on the pool):
/// | Rule | cause | effect |
/// | E1 | a failed resolution with budget remaining | Pending, one attempt spent, retried after the deadline |
/// | E2 | the failed resolution that reaches `max_attempts` | the next tick's retry-exhaustion claim commits `Ended(Indeterminate)` and settles Done |
/// | E3 | throughout | no dead letter; execution never enters |
#[tokio::test]
async fn failed_resolution_attempts_exhaust_into_indeterminate_like_a_crash() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(system_clock()));
    store
        .enqueue(RunDispatch::new(activation_on("failing", "thread-failing")))
        .await
        .unwrap();
    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock.clone(),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            max_attempts: 2,
            ..Default::default()
        },
        Arc::new(ExhaustionResolver {
            terminal_worker: worker_over(text_runtime(), store.clone(), commit.clone()),
            ordinary_calls: ordinary_calls.clone(),
            terminal_calls: terminal_calls.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    )
    .expect("valid test Pool binding");

    assert!(
        wait_for(|| ordinary_calls.load(Ordering::SeqCst) == 1).await,
        "E1 first failed resolution"
    );
    let row = store.list_dispatches().await.unwrap().remove(0);
    assert_eq!(
        (row.state, row.attempt_count),
        (awaken_dispatch::DispatchState::Pending, 1),
        "E1"
    );
    // The first claim epoch's backoff is 250 ms on the store clock.
    clock.set(250);
    assert!(
        wait_for(|| commit.run_state(&RunId("failing".into()))
            == Some(RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Indeterminate
            )))
        .await,
        "E2"
    );
    assert_eq!(ordinary_calls.load(Ordering::SeqCst), 2, "E2 two attempts");
    assert_eq!(terminal_calls.load(Ordering::SeqCst), 1, "E2");
    assert_eq!(store.dispatch_count(), 0, "E2 settled Done");
    assert!(store.dead_letters().await.unwrap().is_empty(), "E3");
    pool.shutdown().await;
}
