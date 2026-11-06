// @formal evidence remote_worker.terminal_scan_is_bounded_and_preserves_authority
//! Scheduling evidence for the one Host/Worker scan, not execution authority.
mod harness;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_dispatch::{Error, MemoryDispatchStore, RunDispatch, TerminalReconciliationScan};

use std::sync::{Arc, Mutex};

#[tokio::test]
async fn inspection_budget_counts_failed_and_nonterminal_rows_and_wraps() {
    // R0 zero budget => zero callbacks. R1 large requested budget => at most
    // 256 inspected IDs, even when the first 120 fail and the next 120 have no
    // terminal result. R2 next page reaches all remaining healthy rows. R3 a
    // short page wraps; resetting process-local cursor rechecks, never claims.
    let store = MemoryDispatchStore::new(awaken_time::system_clock());
    for n in 0..300 {
        store
            .enqueue(RunDispatch::new(harness::activation(&format!(
                "run-{n:03}"
            ))))
            .await
            .unwrap();
    }
    let scan = TerminalReconciliationScan::default();
    let seen = Mutex::new(Vec::new());
    let visit = |row: awaken_dispatch::DispatchSummary| {
        seen.lock().unwrap().push(row.run_id.clone());
        async move {
            if row.run_id.0.as_str() < "run-120" {
                Err(Error::ResolutionNotReady("injected row failure".into()))
            } else if row.run_id.0.as_str() < "run-240" {
                Ok(None)
            } else {
                Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd))))
            }
        }
    };
    assert!(scan.reconcile(&store, 0, &visit).await.unwrap().is_empty());
    assert!(seen.lock().unwrap().is_empty(), "R0");
    assert_eq!(
        scan.reconcile(&store, usize::MAX, &visit)
            .await
            .unwrap()
            .len(),
        16,
        "R1"
    );
    assert_eq!(seen.lock().unwrap().len(), 256, "R1 read budget");
    assert_eq!(
        scan.reconcile(&store, usize::MAX, &visit)
            .await
            .unwrap()
            .len(),
        44,
        "R2"
    );
    assert_eq!(seen.lock().unwrap().len(), 300);
    scan.reconcile(&store, 1, &visit).await.unwrap();
    assert_eq!(seen.lock().unwrap().last().unwrap().0, "run-000", "R3 wrap");
    assert_eq!(store.dispatch_count(), 300, "callbacks confer no authority");
}

#[tokio::test]
async fn concurrent_scan_yields_and_cancelled_page_keeps_cursor_progress() {
    // C1 one scan is inside a row observer; C2 another scan is requested =>
    // E1 no duplicate local observation/no waiter queue. C3 first task aborts
    // before observer returns => E2 its scheduling cursor remains advanced;
    // E3 next scan reaches the suffix, without manufacturing completion.
    // Cross R1 full page (limit=1) and R2 short page (limit=3): abort must
    // retain progress in both, including when normal exhaustion would wrap.
    for (page_limit, row_count) in [(1, 2), (3, 2), (3, 1)] {
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        for run in ["a", "b"].into_iter().take(row_count) {
            store
                .enqueue(RunDispatch::new(harness::activation(run)))
                .await
                .unwrap();
        }
        let scan = Arc::new(TerminalReconciliationScan::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let (scan, store, entered) = (scan.clone(), store.clone(), entered.clone());
            async move {
                scan.reconcile(store.as_ref(), page_limit, |_| {
                    entered.notify_one();
                    std::future::pending()
                })
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
            .await
            .expect("timed out waiting for entered.notified()");
        assert!(
            scan.reconcile(store.as_ref(), 1, |_| async {
                panic!("E1 duplicate observer")
            })
            .await
            .unwrap()
            .is_empty()
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let rows = scan
            .reconcile(store.as_ref(), 1, |row| async move {
                Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd))))
            })
            .await
            .unwrap();
        if row_count == 2 {
            assert_eq!(rows[0].0, RunId("b".into()), "E2 suffix");
        } else {
            // R3 cancellation after the last item of a short page must not
            // wrap early. The next empty read performs the eventual wrap.
            assert!(rows.is_empty(), "last yielded ID retained until next page");
            let retried = scan
                .reconcile(store.as_ref(), 1, |row| async move {
                    Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd))))
                })
                .await
                .unwrap();
            assert_eq!(retried[0].0, RunId("a".into()), "R3 eventual wrap");
        }
        assert_eq!(store.dispatch_count(), row_count, "E3 scan never settles");
    }
}

#[tokio::test]
async fn full_page_end_and_new_process_cursor_never_strand_rows() {
    // R1 exact full page => next empty page resets without observations;
    // R2 next cycle retries first row. R3 process restart loses only schedule:
    // a new cursor also rechecks first row, with all durable records unchanged.
    let store = MemoryDispatchStore::new(awaken_time::system_clock());
    store
        .enqueue(RunDispatch::new(harness::activation("a")))
        .await
        .unwrap();
    let scan = TerminalReconciliationScan::default();
    let visit = |row: awaken_dispatch::DispatchSummary| async move {
        Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd))))
    };
    assert_eq!(scan.reconcile(&store, 1, visit).await.unwrap().len(), 1);
    assert!(
        scan.reconcile(&store, 1, visit).await.unwrap().is_empty(),
        "R1"
    );
    assert_eq!(
        scan.reconcile(&store, 1, visit).await.unwrap().len(),
        1,
        "R2"
    );
    assert_eq!(
        TerminalReconciliationScan::default()
            .reconcile(&store, 1, visit)
            .await
            .unwrap()
            .len(),
        1,
        "R3"
    );
    assert_eq!(store.dispatch_count(), 1);
}

async fn assert_corrupt_summary_scan_isolation(store: &dyn ConformanceDispatch) {
    // SQL scan decision table. C1 first persisted payload cannot decode; C2
    // following payload is healthy; C3 page budget is one versus two. E1 C1
    // consumes a slot without calling the observer or pretending completion.
    // E2 C2 is visited either in the same page (C3=2), or the next page (C3=1).
    // E3 after wrap the corrupt row remains explicit and retryable, not deleted.
    // Constraint: the scan owns scheduling only; no callback has claim authority.
    for budget in [1, 2] {
        let scan = TerminalReconciliationScan::default();
        let seen = Mutex::new(Vec::new());
        let visit = |row: awaken_dispatch::DispatchSummary| {
            seen.lock().unwrap().push(row.run_id.clone());
            async move { Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd)))) }
        };
        let first = scan.reconcile(store, budget, &visit).await.unwrap();
        if budget == 1 {
            assert!(first.is_empty(), "E1 corrupt row spent the only budget");
            assert!(seen.lock().unwrap().is_empty(), "E1 no observer");
            let next = scan.reconcile(store, budget, &visit).await.unwrap();
            assert_eq!(next.len(), 1, "E2 healthy suffix after corrupt page");
        } else {
            assert_eq!(first.len(), 1, "E2 same-page healthy suffix");
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![RunId("healthy".into())],
            "E2 exact observer identity"
        );
        assert!(
            scan.reconcile(store, budget, &visit)
                .await
                .unwrap()
                .is_empty(),
            "full page reaches end"
        );
        assert!(
            store.dispatch_summary(&RunId("bad".into())).await.is_err(),
            "E3 corruption retained"
        );
        assert_eq!(
            store.scan_dispatch_ids(None, 10).await.unwrap().len(),
            2,
            "E3 no mutation"
        );
    }
}

#[tokio::test]
async fn sqlite_scan_counts_corrupt_payload_and_reaches_healthy_suffix() {
    // Matrix S1 executes C1-C3/E1-E3 above against persisted SQLite data.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reconciliation.sqlite");
    let store = awaken_dispatch::SqliteDispatchStore::open(
        path.to_str().unwrap(),
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    for run in ["bad", "healthy"] {
        store
            .enqueue(RunDispatch::new(harness::activation(run)))
            .await
            .unwrap();
    }
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_dispatch SET request = '{}' WHERE run_id = 'bad'")
        .execute(&connection)
        .await
        .unwrap();
    assert_corrupt_summary_scan_isolation(&store).await;
}

#[tokio::test]
async fn postgres_scan_counts_corrupt_payload_and_reaches_healthy_suffix() {
    // Matrix P1 executes the same C1-C3/E1-E3 rules against PostgreSQL JSONB;
    // explicit database configuration makes an unavailable fixture fail closed.
    let Some(pool) = harness::schema_pool("t_reconciliation_bad_payload").await else {
        return;
    };
    let store = awaken_dispatch::PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    for run in ["bad", "healthy"] {
        store
            .enqueue(RunDispatch::new(harness::activation(run)))
            .await
            .unwrap();
    }
    sqlx::query("UPDATE runtime_dispatch SET request = '{}'::jsonb WHERE run_id = 'bad'")
        .execute(&pool)
        .await
        .unwrap();
    assert_corrupt_summary_scan_isolation(&store).await;
}

#[tokio::test]
async fn oversized_adapter_page_cannot_exceed_observation_budget() {
    // C1 adapter ignores requested budget; C2 observer errors or succeeds;
    // C3 limit is zero/one/256/257/MAX. R0 zero -> no reads; R1 every callback
    // spends one slot even on error; R2 300 returned IDs -> <=256 observations;
    // R3 unvisited suffix is reached next call, never skipped or settled.
    for limit in [0, 1, 256, 257, usize::MAX] {
        let store = harness::FlakyDispatchStore::new(0, awaken_time::system_clock())
            .with_oversized_scan_page();
        for n in 0..300 {
            store
                .enqueue(RunDispatch::new(harness::activation(&format!(
                    "run-{n:03}"
                ))))
                .await
                .unwrap();
        }
        let scan = TerminalReconciliationScan::default();
        let seen = Mutex::new(Vec::new());
        let visit = |row: awaken_dispatch::DispatchSummary| {
            seen.lock().unwrap().push(row.run_id.clone());
            async move {
                if row.run_id.0 == "run-000" {
                    Err(Error::ResolutionNotReady(
                        "injected observer failure".into(),
                    ))
                } else {
                    Ok(Some((row.run_id, RunState::Ended(EndCause::NaturalEnd))))
                }
            }
        };
        let budget = limit.min(256);
        assert_eq!(
            scan.reconcile(&store, limit, &visit).await.unwrap().len(),
            budget.saturating_sub(1)
        );
        assert_eq!(seen.lock().unwrap().len(), budget, "R1/R2");
        assert_eq!(
            store.scan_limits(),
            if limit == 0 { vec![] } else { vec![budget] },
            "R0/read cap"
        );
        // Finish only the current traversal, before the next wrap can repeat it.
        for _ in 0..2 {
            if seen.lock().unwrap().len() < 300 {
                scan.reconcile(&store, usize::MAX, &visit).await.unwrap();
            }
        }
        assert_eq!(
            *seen.lock().unwrap(),
            (0..300)
                .map(|n| RunId(format!("run-{n:03}")))
                .collect::<Vec<_>>(),
            "R3 exact suffix"
        );
        assert_eq!(store.dispatch_count(), 300, "no settlement authority");
    }
}

use awaken_dispatch_contract::DispatchEnqueue as _;

use awaken_dispatch_testkit::ConformanceDispatch;

#[tokio::test]
async fn occupied_superseded_and_dead_letters_remain_terminal_scan_candidates() {
    // Causes: C1 original slot occupied versus empty; C2 current public
    // supersession/rejection versus cancelled+expired quarantine; C3 first
    // observer fails; C4 bounded page; C5 an explicitly legacy/inconsistent
    // delivery label is returned by the one existing read-fault adapter.
    // Effects: S1 occupied rows reach the same terminal callback regardless of
    // delivery state; S2 empty quarantined rows remain skipped; S3 a failed
    // candidate spends its budget and later suffix progresses; S4 scan itself
    // never requeues, retires, or manufactures closure/completion. Existing
    // budget/cancellation/corrupt-summary cases cover all iterator mechanics.
    // | Rule | C1 | C2 | C3 | Effect |
    // | S1 | occupied | projected legacy Superseded or real DeadLetter | no/yes | callback + S3 + S4 |
    // | S2 | empty | real Superseded or real DeadLetter | - | no callback + S4 |
    // Constraint: executing exact claim cannot relinquish to Pending; an
    // occupied Superseded row is NOT reachable under current public law.
    // C5 projects only a read label over the real retained Leased slot, never
    // private stored state/history/closure. Underlying list and physical reads
    // prove S4 unchanged. The public occupied supersede attempt remains refused.
    use awaken_dispatch_contract::{
        DispatchAdministration as _, DispatchClaims as _, DispatchControlReads as _,
        DispatchExecution as _, DispatchMaintenance as _, DispatchReads as _,
    };
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = harness::FlakyDispatchStore::new(0, clock.clone())
        .with_summary_delivery_projection(
            RunId("a-occupied-superseded".into()),
            awaken_dispatch::DispatchState::Superseded,
        );
    let mut originals = Vec::new();
    for (run, occupied, superseded) in [
        ("a-occupied-superseded", true, true),
        ("b-empty-superseded", false, true),
        ("c-occupied-dead", true, false),
        ("d-empty-dead", false, false),
    ] {
        let thread = format!("thread-{run}");
        let run_id = RunId(run.into());
        store
            .enqueue(RunDispatch::new(harness::activation_on(run, &thread)))
            .await
            .unwrap();
        let initial = store
            .claim_run(&run_id, "original-owner", 1, &Default::default())
            .await
            .unwrap()
            .unwrap();
        if occupied {
            store
                .begin_attempt(&awaken_dispatch::RunClaim::from(&initial.lease), None)
                .await
                .unwrap();
        }
        if superseded {
            let relinquished = store
                .relinquish_claim(&awaken_dispatch::RunClaim::from(&initial.lease))
                .await
                .unwrap();
            assert_eq!(
                relinquished,
                if occupied {
                    awaken_dispatch::SettleOutcome::Fenced
                } else {
                    awaken_dispatch::SettleOutcome::Applied
                },
                "public relinquish does not erase occupancy"
            );
            store
                .enqueue_with(
                    RunDispatch::new(harness::activation_on(&format!("successor-{run}"), &thread)),
                    awaken_dispatch::SubmitOptions {
                        supersede: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        } else {
            store.cancel(&run_id).await.unwrap();
            let next = store
                .claim_run(&run_id, "quarantine-owner", 1, &Default::default())
                .await
                .unwrap()
                .unwrap();
            clock.set(next.lease.expires_ms() + 1);
            assert_eq!(
                store
                    .quarantine_retry_exhausted(0, next.lease.expires_ms() + 1)
                    .await
                    .unwrap(),
                1
            );
        }
        let actual = store
            .list_dispatches()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.run_id == run_id)
            .unwrap();
        assert_eq!(
            actual.state,
            if superseded && occupied {
                awaken_dispatch::DispatchState::Leased
            } else if superseded {
                awaken_dispatch::DispatchState::Superseded
            } else {
                awaken_dispatch::DispatchState::DeadLetter
            },
            "current public transition truth"
        );
        assert_eq!(actual.physical_attempt_active, occupied);
        assert_eq!(actual.cancellation_requested, !superseded);
        let summary = store.dispatch_summary(&run_id).await.unwrap().unwrap();
        assert_eq!(summary.physical_attempt_active, occupied);
        assert_eq!(
            summary.state,
            if superseded {
                awaken_dispatch::DispatchState::Superseded
            } else {
                awaken_dispatch::DispatchState::DeadLetter
            }
        );
        originals.push((
            run_id,
            summary,
            actual,
            store
                .load_physical_attempt(&RunId(run.into()))
                .await
                .unwrap(),
        ));
    }
    let scan = TerminalReconciliationScan::default();
    let seen = Mutex::new(Vec::new());
    for _ in 0..6 {
        scan.reconcile(&store, 1, |row| {
            seen.lock().unwrap().push(row.run_id.clone());
            async move {
                if row.run_id.0 == "a-occupied-superseded" {
                    Err(Error::ResolutionNotReady(
                        "first orphan source unavailable".into(),
                    ))
                } else {
                    Ok(None)
                }
            }
        })
        .await
        .unwrap();
    }
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.contains(&RunId("a-occupied-superseded".into())),
        "S1/S3 failed first orphan visited"
    );
    assert!(
        seen.contains(&RunId("c-occupied-dead".into())),
        "S1/S3 later orphan not starved"
    );
    for (run, summary, actual, physical) in originals {
        assert_eq!(
            store
                .list_dispatches()
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.run_id == run)
                .unwrap(),
            actual,
            "S4 real stored state unchanged by projection/scan"
        );
        assert_eq!(
            store.dispatch_summary(&run).await.unwrap(),
            Some(summary.clone()),
            "S4"
        );
        assert_eq!(
            store.load_physical_attempt(&run).await.unwrap(),
            physical,
            "S4"
        );
        if !summary.physical_attempt_active {
            assert!(!seen.contains(&run), "S2");
        }
    }
}
