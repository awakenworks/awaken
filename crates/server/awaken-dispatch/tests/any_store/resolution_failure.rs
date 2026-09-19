use super::*;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{DispatchWorker, ManualClock};
use awaken_store_inmem::MemoryCommitCoordinator;

// Cause/effect decision table for the canonical pre-execution failure commit.
// C1 deterministic resolution failure; C2 storage rejects before commit;
// C3 storage commits but loses reply; C4 claim epoch/owner is replaced.
// F1 C1+C2 => no terminal lie, retain queue head, then exact retry can settle.
// F2 C1+C3 => read committed truth, settle once without a duplicate commit.
// F3 C1+C4 => old failure cannot commit/settle the successor; new owner can.
// All rules: immutable envelope remains untouched; no model/tool invocation;
// terminal truth precedes queue release; a same-Thread follower is then claimable.
// Run against both Memory and actual SQLite dispatch adapters. This is not a
// claim that PostgreSQL or the external Control transport ran in this test.
#[tokio::test]
async fn terminal_resolution_storage_faults_and_takeover_preserve_queue_progress() {
    for sqlite in [false, true] {
        for fault in ["reject", "lost-reply", "takeover"] {
            let clock = Arc::new(ManualClock::new(100));
            let store = Arc::new(if sqlite {
                any_in_memory_with_clock_handle(clock.clone())
            } else {
                AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(clock.clone())))
            });
            let committed = Arc::new(MemoryCommitCoordinator::new());
            exercise_failure(
                store,
                committed.clone(),
                || committed.commit_count(),
                fault,
                clock,
            )
            .await;
            // R19 the SAME fault table also commits through a file-backed
            // ThreadCommit authority; reopening must retain its one terminal
            // fact after a lost reply, not just a process-local projection.
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("commit.db");
            let committed = Arc::new(
                awaken_store_sqlite::SqliteCommitCoordinator::open(database.to_str().unwrap())
                    .unwrap(),
            );
            let clock = Arc::new(ManualClock::new(100));
            let store = Arc::new(if sqlite {
                any_in_memory_with_clock_handle(clock.clone())
            } else {
                AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(clock.clone())))
            });
            exercise_failure(
                store,
                committed.clone(),
                || committed.commit_count(),
                fault,
                clock,
            )
            .await;
            drop(committed);
            let reopened =
                awaken_store_sqlite::SqliteCommitCoordinator::open(database.to_str().unwrap())
                    .unwrap();
            assert_eq!(reopened.commit_count(), 1, "R19 durable terminal count");
            assert!(matches!(
                reopened.run_state(&awaken_agent_contract::agent::run::Id("conflict".into())),
                Some(RunState::Ended(_))
            ));
        }
    }
}

async fn exercise_failure<C>(
    store: Arc<AnyDispatchStore>,
    committed: Arc<C>,
    count: impl Fn() -> u64,
    fault: &str,
    clock: Arc<ManualClock>,
) where
    C: awaken_agent_contract::thread::commit::coordinator::Coordinator
        + CommittedThreadView
        + awaken_agent_contract::thread::read::recovery::RunRecoverySource
        + 'static,
{
    let boundary = Arc::new(harness::FailingCommit::new(
        committed.clone(),
        fault == "reject",
    ));
    if fault == "lost-reply" {
        boundary.lose_next_response();
    }
    let (runtime, model_calls) = harness::counting_text_runtime();
    let worker = DispatchWorker::new(runtime, store.clone(), boundary.clone(), "owner-a");
    clock.set(100);
    let first = activation_on("conflict", "one-thread");
    let first_id = first.run_id.clone();
    store.enqueue(RunDispatch::new(first)).await.unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("follower", "one-thread")))
        .await
        .unwrap();
    clock.set(100);
    let claimed = store
        .claim("owner-a", 1_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.set(100);
    assert!(
        store
            .claim("peer", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    let successor = if fault == "takeover" {
        clock.advance(1_001);
        clock.set(1_101);
        Some(
            store
                .claim("owner-b", 1_000, &Default::default())
                .await
                .unwrap()
                .unwrap(),
        )
    } else {
        None
    };
    let result = worker
        .fail_claimed_before_execution(
            &claimed,
            "dispatch_resolution_failed",
            "immutable Resource snapshot conflict",
        )
        .await;
    if fault == "lost-reply" {
        assert!(
            result.unwrap().is_some(),
            "F2 durable truth settles a lost reply"
        );
    } else {
        assert!(
            result.is_err() || result.unwrap().is_none(),
            "F1/F3 cannot claim terminal success"
        );
        assert!(committed.run_state(&first_id).is_none());
        assert_eq!(count(), 0);
        boundary.set_failing(false);
        let retry = successor.as_ref().unwrap_or(&claimed);
        assert!(
            worker
                .fail_claimed_before_execution(
                    retry,
                    "dispatch_resolution_failed",
                    "immutable Resource snapshot conflict"
                )
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(count(), 1, "one terminal commit, including lost reply");
    assert!(matches!(
        committed.run_state(&first_id),
        Some(RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::Error(_)
        ))
    ));
    assert_eq!(model_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    clock.set(1_101);
    let follower = store
        .claim("peer", 1_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(follower.request.run_id().0, "follower");
}
