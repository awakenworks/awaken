//! Current-state reads share one projection and never authorize execution.

mod harness;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch::{
    AnyCoordinatorDispatch, DispatchOutcome, MemoryDispatchStore, PostgresDispatchStore,
    RunDispatch, SqliteDispatchStore,
};
use awaken_dispatch_contract::{
    DispatchControlReads, DispatchCursor, DispatchError, DispatchOperationalFeed,
    SessionRunAdmissionObservation, SessionRunAdmissionReads, SessionRunReservations,
};

fn id(value: &str) -> RunId {
    RunId(value.into())
}

async fn assert_bounded_query_conformance(
    store: &(impl ConformanceDispatch + DispatchOperationalFeed),
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
    // Admission observation cause/effect rules use these real persisted inputs:
    // O1 Missing -> read-only, not reservation; O2 retained original row -> exact
    // Live, repeated reads do not change lease/attempt/credentials/state/cursor;
    // O3 Done -> original completion only; O4 legacy envelope None stays None.
    // O6 Missing/read then concurrent A insert -> different B still conflicts;
    // O7 Live/read then Done -> exact A still replays its original tombstone;
    // O8 changed full input never borrows root SessionCommand identity.
    // O5 malformed/legacy tombstone and O9 error propagation/type exclusion are
    // owned by the adjacent real corruption cases and original Worker doctest.
    // O10 C1 this exact store is viewed through its read-only admission role;
    // C2 Missing/Live/Completed/legacy inputs still yield O1-O4 with unchanged
    // rows/cursors. E1 the reader uses the original snapshot, E2 it cannot
    // reserve/activate/reject/enqueue (the contract's compile-fail test), and E3
    // the Worker view cannot acquire it (the existing erasure compile-fail).
    let admission_reads: &dyn SessionRunAdmissionReads = store;
    let empty_rows = store.list_dispatches().await.unwrap();
    let empty_feed = store
        .events_after(DispatchCursor::default(), 128, None)
        .await
        .unwrap();
    let empty_completion = store.completion_events_after(0, 128).await.unwrap();
    assert_eq!(
        admission_reads
            .observe_session_run_admission(&id("query-c"))
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Missing,
        "O1"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        empty_rows,
        "O1 no row"
    );
    assert_eq!(
        store
            .events_after(DispatchCursor::default(), 128, None)
            .await
            .unwrap(),
        empty_feed,
        "O1 no cursor/event"
    );
    assert_eq!(
        store.completion_events_after(0, 128).await.unwrap(),
        empty_completion,
        "O1 no completion"
    );
    let originals =
        ["query-c", "query-a", "query-e"].map(|run| RunDispatch::new(harness::activation(run)));
    for request in &originals {
        store.enqueue(request.clone()).await.unwrap();
    }
    let before = store.list_dispatches().await.unwrap();
    let feed_before = store
        .events_after(DispatchCursor::default(), 128, None)
        .await
        .unwrap();
    for request in &originals {
        for _ in 0..2 {
            assert_eq!(
                admission_reads
                    .observe_session_run_admission(request.run_id())
                    .await
                    .unwrap(),
                SessionRunAdmissionObservation::Live(Box::new(request.clone())),
                "O2/O4"
            );
        }
    }
    assert_eq!(store.list_dispatches().await.unwrap(), before, "O2 stutter");
    assert_eq!(
        store
            .events_after(DispatchCursor::default(), 128, None)
            .await
            .unwrap(),
        feed_before,
        "O2 no new cursor/event"
    );
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
    assert_eq!(claimed.lease.run_id(), &id("query-c"));
    let leased = store.dispatch_summary(&id("query-c")).await.unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(&id("query-c"))
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Live(Box::new(claimed.request.clone())),
        "O2 already leased"
    );
    assert_eq!(
        store.dispatch_summary(&id("query-c")).await.unwrap(),
        leased,
        "O2 read changes no claim/attempt"
    );
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
                claimed.lease.run_id(),
                claimed.lease.epoch(),
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
    // O7 actual completion between an observed A and its exact replay.
    let completion = store
        .completion_events_after(0, 128)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == id("query-c"))
        .unwrap();
    let feed_done = store
        .events_after(DispatchCursor::default(), 128, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(&id("query-c"))
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Completed(completion.clone()),
        "O3"
    );
    store.enqueue(claimed.request.clone()).await.unwrap();
    assert_eq!(
        store
            .observe_session_run_admission(&id("query-c"))
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Completed(completion),
        "O7 original receipt"
    );
    assert_eq!(
        store
            .events_after(DispatchCursor::default(), 128, None)
            .await
            .unwrap(),
        feed_done,
        "O3/O7 no event/cursor on read/replay"
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
    assert_eq!(
        store
            .observe_session_run_admission(&id("query-race"))
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Missing,
        "O6 not a reservation"
    );
    let a = RunDispatch::new(harness::activation("query-race"));
    store.enqueue(a.clone()).await.unwrap();
    let before_race = store.list_dispatches().await.unwrap();
    let cursor_race = store
        .events_after(DispatchCursor::default(), 128, None)
        .await
        .unwrap();
    let mut b = a.clone();
    b.activation
        .snapshot
        .resolved_spec
        .instructions
        .push_str("changed");
    assert!(
        matches!(store.enqueue(b).await, Err(DispatchError::Conflict(_))),
        "O6/O8"
    );
    assert_eq!(
        store
            .observe_session_run_admission(a.run_id())
            .await
            .unwrap(),
        SessionRunAdmissionObservation::Live(Box::new(a)),
        "O6 original A"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        before_race,
        "O6/O8 stutter"
    );
    assert_eq!(
        store
            .events_after(DispatchCursor::default(), 128, None)
            .await
            .unwrap(),
        cursor_race,
        "O6/O8 cursor stutter"
    );
}

#[tokio::test]
async fn memory_and_any_bounded_dispatch_queries_conform() {
    // Backend matrix Q1-Q6 above: Any must forward the same bounded port rather
    // than silently using a full-list fallback; Memory owns one ordered row map.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    assert_bounded_query_conformance(
        &AnyCoordinatorDispatch::from_durable_dispatch(std::sync::Arc::new(
            MemoryDispatchStore::new(clock.clock()),
        )),
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
        &SqliteDispatchStore::open_in_memory(clock.clock())
            .await
            .unwrap(),
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

async fn assert_bad_row_isolation(
    store: &(impl awaken_dispatch::PhysicalAttemptReads + SessionRunReservations),
) {
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
    // R7 one corrupt Dispatch row makes owner occupancy undecidable; the
    // shared release/observation scan must propagate that error instead of
    // treating the owner as empty, even if a healthy peer remains readable.
    assert!(
        awaken_dispatch::has_active_physical_attempt_for_owner(store, "owner:1:boot")
            .await
            .is_err(),
        "R7 corrupt occupancy is not an empty observation"
    );
}

#[tokio::test]
async fn sqlite_corrupt_summary_does_not_poison_identity_pages() {
    // Q7 uses a real persisted payload corruption, not a mock query result.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queries.sqlite");
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), awaken_time::system_clock())
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
    assert_bad_row_isolation(&store).await;
    // O5/O9 preserve existing decoder error; observation key mismatch is not
    // another request. O3/O5 completion full-decoding rejects negative sequence,
    // while original exact replay still requires ONLY its original fingerprint.
    // Legacy affinity/fingerprint None is retained, never filled or authorized.
    let wrong = RunDispatch::new(harness::activation("other-key"));
    sqlx::query("UPDATE runtime_dispatch SET request = ?1 WHERE run_id = 'bad'")
        .bind(serde_json::to_string(&wrong).unwrap())
        .execute(&connection)
        .await
        .unwrap();
    assert!(
        matches!(
            store.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O5 key mismatch"
    );
    let mut wrong_thread = RunDispatch::new(harness::activation("bad"));
    wrong_thread.activation.thread_id =
        awaken_agent_contract::agent::thread::Id("other-thread".into());
    sqlx::query("UPDATE runtime_dispatch SET request = ?1 WHERE run_id = 'bad'")
        .bind(serde_json::to_string(&wrong_thread).unwrap())
        .execute(&connection)
        .await
        .unwrap();
    assert!(
        matches!(
            store.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O5 Thread key mismatch, exact Run key"
    );
    let legacy = RunDispatch::new(harness::activation("negative-sequence"));
    sqlx::query("INSERT INTO runtime_dispatch_completion (sequence, run_id, request_fingerprint) VALUES (-1, 'negative-sequence', ?1)")
        .bind(legacy.canonical_fingerprint()).execute(&connection).await.unwrap();
    assert!(
        matches!(store.observe_session_run_admission(legacy.run_id()).await,
        Err(DispatchError::Rejected(message)) if message == "persisted completion sequence is negative"),
        "O5 original full completion decoder error"
    );
    store.enqueue(legacy).await.unwrap();
    assert!(
        store
            .dispatch_summary(&id("negative-sequence"))
            .await
            .unwrap()
            .is_none(),
        "O5 old replay ignores new completion metadata and does not append"
    );
    sqlx::query("INSERT INTO runtime_dispatch_completion (run_id) VALUES ('legacy-none')")
        .execute(&connection)
        .await
        .unwrap();
    let observed = store
        .observe_session_run_admission(&id("legacy-none"))
        .await
        .unwrap();
    let SessionRunAdmissionObservation::Completed(legacy_none) = observed else {
        panic!("O3 legacy completion is not Missing or full input");
    };
    assert_eq!(legacy_none.request_fingerprint, None, "O3 original None");
    assert_eq!(legacy_none.thread_id, None, "O3 original None");
    assert_eq!(legacy_none.session_thread_id, None, "O3 original None");
    assert!(
        matches!(
            store
                .enqueue(RunDispatch::new(harness::activation("legacy-none")))
                .await,
            Err(DispatchError::Conflict(_))
        ),
        "O3 old unknown cannot authorize replay"
    );

    let coordinator = AnyCoordinatorDispatch::from_durable_store(store, None);
    assert!(
        matches!(
            coordinator.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O9 erased owner preserves key/decode errors"
    );
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
    // O5/O9 preserve existing decoder error; observation key mismatch is not
    // another request. O3/O5 completion full-decoding rejects negative sequence,
    // while original exact replay still requires ONLY its original fingerprint.
    // Legacy affinity/fingerprint None is retained, never filled or authorized.
    let wrong = RunDispatch::new(harness::activation("other-key"));
    sqlx::query("UPDATE runtime_dispatch SET request = $1::TEXT::jsonb WHERE run_id = 'bad'")
        .bind(serde_json::to_string(&wrong).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(
            store.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O5 key mismatch"
    );
    let mut wrong_thread = RunDispatch::new(harness::activation("bad"));
    wrong_thread.activation.thread_id =
        awaken_agent_contract::agent::thread::Id("other-thread".into());
    sqlx::query("UPDATE runtime_dispatch SET request = $1::TEXT::jsonb WHERE run_id = 'bad'")
        .bind(serde_json::to_string(&wrong_thread).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(
            store.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O5 Thread key mismatch, exact Run key"
    );
    let legacy = RunDispatch::new(harness::activation("negative-sequence"));
    sqlx::query("INSERT INTO runtime_dispatch_completion (sequence, run_id, request_fingerprint) VALUES (-1, 'negative-sequence', $1)")
        .bind(legacy.canonical_fingerprint()).execute(&pool).await.unwrap();
    assert!(
        matches!(store.observe_session_run_admission(legacy.run_id()).await,
        Err(DispatchError::Rejected(message)) if message == "persisted completion sequence is negative"),
        "O5 original full completion decoder error"
    );
    store.enqueue(legacy).await.unwrap();
    assert!(
        store
            .dispatch_summary(&id("negative-sequence"))
            .await
            .unwrap()
            .is_none(),
        "O5 old replay ignores new completion metadata and does not append"
    );
    sqlx::query("INSERT INTO runtime_dispatch_completion (run_id) VALUES ('legacy-none')")
        .execute(&pool)
        .await
        .unwrap();
    let observed = store
        .observe_session_run_admission(&id("legacy-none"))
        .await
        .unwrap();
    let SessionRunAdmissionObservation::Completed(legacy_none) = observed else {
        panic!("O3 legacy completion is not Missing or full input");
    };
    assert_eq!(legacy_none.request_fingerprint, None, "O3 original None");
    assert_eq!(legacy_none.thread_id, None, "O3 original None");
    assert_eq!(legacy_none.session_thread_id, None, "O3 original None");
    assert!(
        matches!(
            store
                .enqueue(RunDispatch::new(harness::activation("legacy-none")))
                .await,
            Err(DispatchError::Conflict(_))
        ),
        "O3 old unknown cannot authorize replay"
    );

    let coordinator = AnyCoordinatorDispatch::from_durable_store(store, None);
    assert!(
        matches!(
            coordinator.observe_session_run_admission(&id("bad")).await,
            Err(DispatchError::Unavailable(_))
        ),
        "O9 erased owner preserves key/decode errors"
    );
}

use awaken_dispatch_contract::DispatchEnqueue as _;

use awaken_dispatch_testkit::ConformanceDispatch;
