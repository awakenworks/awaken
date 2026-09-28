// @formal evidence remote_worker.terminal_scan_is_bounded_and_preserves_authority
//! Scheduling evidence for the one Host/Worker scan, not execution authority.
mod harness;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_dispatch::{
    DispatchQueue, Error, MemoryDispatchStore, RunDispatch, TerminalReconciliationScan,
};
use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn inspection_budget_counts_failed_and_nonterminal_rows_and_wraps() {
    // R0 zero budget => zero callbacks. R1 large requested budget => at most
    // 256 inspected IDs, even when the first 120 fail and the next 120 have no
    // terminal result. R2 next page reaches all remaining healthy rows. R3 a
    // short page wraps; resetting process-local cursor rechecks, never claims.
    let store = MemoryDispatchStore::new(awaken_lease_contract::system_clock());
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
        let store = Arc::new(MemoryDispatchStore::new(
            awaken_lease_contract::system_clock(),
        ));
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
    let store = MemoryDispatchStore::new(awaken_lease_contract::system_clock());
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

async fn assert_corrupt_summary_scan_isolation(store: &dyn ControlDispatchQueue) {
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
        awaken_lease_contract::system_clock(),
    )
    .unwrap();
    for run in ["bad", "healthy"] {
        store
            .enqueue(RunDispatch::new(harness::activation(run)))
            .await
            .unwrap();
    }
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute(
            "UPDATE runtime_dispatch SET request = '{}' WHERE run_id = 'bad'",
            [],
        )
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
        let store = harness::FlakyDispatchStore::new(0, awaken_lease_contract::system_clock())
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
