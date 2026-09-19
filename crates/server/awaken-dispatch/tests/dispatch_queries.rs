//! Current-state reads share one projection and never authorize execution.

mod harness;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch::{
    AnyDispatchStore, DispatchOutcome, DispatchQueue, MemoryDispatchStore, PostgresDispatchStore,
    RunDispatch, SqliteDispatchStore,
};

fn id(value: &str) -> RunId {
    RunId(value.into())
}

async fn assert_bounded_query_conformance(
    store: &dyn DispatchQueue,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // Cause/effect decision table. C1 page budget is zero/one/multiple; C2 the
    // cursor exists/is deleted/is absent; C3 peers are inserted before/after it;
    // C4 a row changes lifecycle; C5 the exact row exists/is absent.
    // Q1 C1=0 -> zero identities, no mutation. Q2 C1>0 -> sorted exclusive
    // page with at most budget identities, not enqueue order. Q3 C2=deleted or
    // absent -> continue strictly after the value, not an offset. Q4 C3=before
    // -> discovered after wrap; C3=after -> discovered in this pass. Q5 C4 ->
    // exact projection matches the original query with no state filtering.
    // Q6 C5=absent -> None, without interpreting absence as physical stopping.
    // Constraint: pages are scheduling hints, not snapshots or transition rights.
    for run in ["query-c", "query-a", "query-e"] {
        store
            .enqueue(RunDispatch::new(harness::activation(run)))
            .await
            .unwrap();
    }
    let before = store.list_dispatches().await.unwrap();
    assert!(
        store.scan_dispatch_ids(None, 0).await.unwrap().is_empty(),
        "Q1"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        before,
        "Q1 no mutation"
    );
    assert_eq!(
        store.scan_dispatch_ids(None, 1).await.unwrap(),
        vec![id("query-a")],
        "Q2"
    );
    assert_eq!(
        store.scan_dispatch_ids(None, 2).await.unwrap(),
        vec![id("query-a"), id("query-c")],
        "Q2"
    );
    assert_eq!(
        store
            .scan_dispatch_ids(Some(&id("query-b")), 2)
            .await
            .unwrap(),
        vec![id("query-c"), id("query-e")],
        "Q3 absent cursor"
    );
    for row in &before {
        assert_eq!(
            store.dispatch_summary(&row.run_id).await.unwrap().as_ref(),
            Some(row),
            "Q5 exact canonical projection"
        );
    }
    clock.set(0);
    let claimed = store
        .claim("query-owner", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.lease.run_id, id("query-c"));
    assert_eq!(
        store.dispatch_summary(&id("query-c")).await.unwrap(),
        store
            .list_dispatches()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.run_id == id("query-c")),
        "Q5 leased projection"
    );
    assert!(
        store
            .settle(
                &claimed.lease.run_id,
                claimed.lease.epoch,
                DispatchOutcome::Done,
                &[]
            )
            .await
            .unwrap()
            .applied()
    );
    assert!(
        store
            .dispatch_summary(&id("query-c"))
            .await
            .unwrap()
            .is_none(),
        "Q6 deleted"
    );
    for run in ["query-b", "query-d"] {
        store
            .enqueue(RunDispatch::new(harness::activation(run)))
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .scan_dispatch_ids(Some(&id("query-c")), 2)
            .await
            .unwrap(),
        vec![id("query-d"), id("query-e")],
        "Q3/Q4 deleted cursor and insertion"
    );
    assert!(
        store
            .scan_dispatch_ids(Some(&id("query-e")), 2)
            .await
            .unwrap()
            .is_empty(),
        "Q2 end"
    );
    assert_eq!(
        store.scan_dispatch_ids(None, 2).await.unwrap(),
        vec![id("query-a"), id("query-b")],
        "Q4 wrap finds insertion behind cursor"
    );
    assert!(
        store
            .dispatch_summary(&id("query-missing"))
            .await
            .unwrap()
            .is_none(),
        "Q6 missing"
    );
}

#[tokio::test]
async fn memory_and_any_bounded_dispatch_queries_conform() {
    // Backend matrix Q1-Q6 above: Any must forward the same bounded port rather
    // than silently using a full-list fallback; Memory owns one ordered row map.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    assert_bounded_query_conformance(
        &AnyDispatchStore::from_dispatch(std::sync::Arc::new(MemoryDispatchStore::new(
            clock.clock(),
        ))),
        &clock,
    )
    .await;
}

#[tokio::test]
async fn sqlite_bounded_dispatch_queries_conform() {
    // Backend matrix Q1-Q6 above: SQLite uses its existing primary key and exact
    // projection decoder; the budget applies before payload deserialization.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    assert_bounded_query_conformance(
        &SqliteDispatchStore::open_in_memory(clock.clock()).unwrap(),
        &clock,
    )
    .await;
}

#[tokio::test]
async fn postgres_bounded_dispatch_queries_conform() {
    // Backend matrix Q1-Q6 above: explicit AWAKEN_TEST_DATABASE_URL makes
    // connection failure fail closed in the shared harness, never a false pass.
    let Some(pool) = harness::schema_pool("t_dispatch_bounded_queries").await else {
        return;
    };
    assert_bounded_query_conformance(
        &PostgresDispatchStore::with_pool(pool).await.unwrap(),
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
}

async fn assert_bad_row_isolation(store: &dyn DispatchQueue) {
    // Q7 malformed request -> identity is still discoverable, exact summary
    // errors (not None), and a healthy peer remains readable. This ensures the
    // caller can advance its cursor and isolate corruption without hiding it.
    assert_eq!(
        store.scan_dispatch_ids(None, 1).await.unwrap(),
        vec![id("bad")]
    );
    assert!(
        store.dispatch_summary(&id("bad")).await.is_err(),
        "Q7 reject corruption"
    );
    assert_eq!(
        store.scan_dispatch_ids(Some(&id("bad")), 1).await.unwrap(),
        vec![id("healthy")]
    );
    assert!(
        store
            .dispatch_summary(&id("healthy"))
            .await
            .unwrap()
            .is_some(),
        "Q7 healthy peer"
    );
    assert!(
        store.list_dispatches().await.is_err(),
        "Q7 original monitoring decoder remains strict"
    );
}

#[tokio::test]
async fn sqlite_corrupt_summary_does_not_poison_identity_pages() {
    // Q7 uses a real persisted payload corruption, not a mock query result.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queries.sqlite");
    let store = SqliteDispatchStore::open(
        path.to_str().unwrap(),
        std::sync::Arc::new(awaken_dispatch::SystemClock),
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
    assert_bad_row_isolation(&store).await;
}

#[tokio::test]
async fn postgres_corrupt_summary_does_not_poison_identity_pages() {
    // Q7 applies the same malformed request to PostgreSQL JSONB: identity-page
    // iteration must not decode unrelated payloads or turn corruption into None.
    let Some(pool) = harness::schema_pool("t_dispatch_bad_summary").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
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
    assert_bad_row_isolation(&store).await;
}
