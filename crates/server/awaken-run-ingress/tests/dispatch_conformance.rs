use std::sync::Arc;

use awaken_run_ingress::{
    ManualClock, MemoryDispatchStore, PostgresDispatchStore, SqliteDispatchStore,
};
use awaken_run_ingress_testkit::{
    ConformanceCapabilities, assert_atomic_report_continuation_conformance,
    assert_dispatch_conformance, assert_dispatch_conformance_with_clock,
    assert_session_reply_activity_rotation_conformance,
};
use awaken_run_ingress_testkit::{
    assert_dispatch_operational_feed_conformance,
    assert_dispatch_operational_feed_conformance_with_clock, record_dispatch_recovery_history,
};

mod harness;

#[tokio::test]
async fn memory_dispatch_conforms() {
    // Coverage rationale. Causes: the canonical conformance matrix executes
    // against the in-memory backend. Effects: every shared dispatch, atomic
    // continuation, and operational-feed rule must pass. Constraint/Invariant:
    // Memory has no backend-only behavior. Decision rule: bind matrix row M1 to
    // the three authoritative helpers rather than duplicating their cases here.
    let clock = Arc::new(ManualClock::new(0));
    let store = MemoryDispatchStore::new().with_clock(clock.clone());
    let set_clock = move |now_ms| clock.set(now_ms);
    assert_dispatch_conformance_with_clock(
        &store,
        "conformance-memory",
        ConformanceCapabilities::LOCAL_STORE,
        &set_clock,
    )
    .await;
    assert_atomic_report_continuation_conformance(&store, "conformance-memory-report").await;
    assert_dispatch_operational_feed_conformance_with_clock(
        &store,
        "conformance-memory",
        &set_clock,
    )
    .await;
}

#[tokio::test]
async fn memory_session_reply_activity_rotation_conforms() {
    // Coverage rationale: Memory is rule M1 in the backend matrix. It must
    // satisfy the shared C1-C6/AR0-AR9 decision table and all E1-E10 effects
    // documented on the canonical conformance helper, without a backend-only
    // staging or activity-rotation path.
    // Causes: Memory is selected and each shared reply/admission partition is
    // exercised. Effects: E1-E10 are inherited from that helper. Constraint/
    // Invariant: activity rotation is atomic with the existing row. Decision
    // rule: matrix row M1 delegates once to the canonical AR0-AR9 table.
    let store = MemoryDispatchStore::new();
    assert_session_reply_activity_rotation_conformance(&store, "conformance-memory-reply").await;
}

#[tokio::test]
async fn sqlite_dispatch_conforms() {
    // Coverage rationale. Causes: the canonical conformance matrix executes
    // against an in-memory SQLite backend. Effects: every shared dispatch,
    // atomic-continuation, durable-replay, and feed rule must pass. Constraint/
    // Invariant: SQL persistence cannot change neutral semantics. Decision rule:
    // bind matrix row S1 to the authoritative helpers without copying cases.
    let clock = Arc::new(ManualClock::new(0));
    let store = SqliteDispatchStore::open_in_memory()
        .expect("open sqlite conformance store")
        .with_clock(clock.clone());
    let set_clock = move |now_ms| clock.set(now_ms);
    assert_dispatch_conformance_with_clock(
        &store,
        "conformance-sqlite",
        ConformanceCapabilities::LOCAL_STORE,
        &set_clock,
    )
    .await;
    assert_atomic_report_continuation_conformance(&store, "conformance-sqlite-report").await;
    assert_dispatch_operational_feed_conformance_with_clock(
        &store,
        "conformance-sqlite",
        &set_clock,
    )
    .await;
}

#[tokio::test]
async fn sqlite_session_reply_activity_rotation_conforms() {
    // Coverage rationale: SQLite is rule S1 in the backend matrix. It must
    // satisfy the shared C1-C6/AR0-AR9 decision table and all E1-E10 effects
    // documented on the canonical conformance helper, including durable replay
    // after the activity epoch rotates.
    // Causes: SQLite is selected and each shared reply/admission partition is
    // exercised. Effects: E1-E10 and durable replay follow the helper. Constraint/
    // Invariant: rotation and staged input share one SQL transaction. Decision
    // rule: matrix row S1 delegates once to the canonical AR0-AR9 table.
    let store = SqliteDispatchStore::open_in_memory().expect("open sqlite reply conformance store");
    assert_session_reply_activity_rotation_conformance(&store, "conformance-sqlite-reply").await;
}

#[tokio::test]
async fn memory_and_sqlite_refine_the_same_crash_recovery_history() {
    /* Differential/metamorphic design: C1 one canonical predecessor-crash
     * scenario runs against Memory; C2 the same normalized scenario runs
     * against SQLite. E1 each history independently satisfies the fenced
     * recovery oracle; E2 the normalized histories are identical. Constraint:
     * namespace and physical timestamps are excluded from the comparison, but
     * epochs, admissions, fencing, settlement and final liveness are retained.
     * Decision D1=C1+C2->E1+E2. */
    let memory_clock = Arc::new(ManualClock::new(0));
    let sqlite_clock = Arc::new(ManualClock::new(0));
    let memory = MemoryDispatchStore::new().with_clock(memory_clock.clone());
    let sqlite = SqliteDispatchStore::open_in_memory()
        .expect("open SQLite history store")
        .with_clock(sqlite_clock.clone());
    let set_memory_clock = |now_ms| memory_clock.set(now_ms);
    let set_sqlite_clock = |now_ms| sqlite_clock.set(now_ms);
    let memory_history =
        record_dispatch_recovery_history(&memory, "history-memory", &set_memory_clock).await;
    let sqlite_history =
        record_dispatch_recovery_history(&sqlite, "history-sqlite", &set_sqlite_clock).await;
    assert_eq!(memory_history, sqlite_history, "D1/E2 backend refinement");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_dispatch_conforms() {
    // Coverage rationale. Causes: C1 PostgreSQL is reachable; C2 it is absent in
    // this environment. Effects: E1 C1 must pass every canonical dispatch,
    // continuation, and feed rule; E2 C2 skips environmental acceptance.
    // Constraint/Invariant: PostgreSQL must match the neutral store contract.
    // Decision rule: bind matrix row P1 to the authoritative helpers when C1.
    let Some(pool) = harness::schema_pool("t_dispatch_conformance").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool)
        .await
        .expect("open postgres conformance store");
    assert_dispatch_conformance(
        &store,
        "conformance-postgres",
        ConformanceCapabilities::LOCAL_STORE,
    )
    .await;
    assert_atomic_report_continuation_conformance(&store, "conformance-postgres-report").await;
    assert_dispatch_operational_feed_conformance(&store, "conformance-postgres").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_reply_activity_rotation_conforms() {
    // Coverage rationale. Causes: C1 PostgreSQL is reachable and the shared
    // AR0-AR9 reply/admission matrix runs; C2 it is unavailable. Effects: E1 C1
    // yields canonical E1-E10; E2 C2 skips environmental acceptance. Constraint/
    // Invariant: reply input and activity rotation commit in one transaction.
    // Decision rule: matrix row P1 delegates once to the canonical helper on C1.
    let Some(pool) = harness::schema_pool("t_dispatch_reply_activity").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool)
        .await
        .expect("open postgres reply conformance store");
    assert_session_reply_activity_rotation_conformance(&store, "conformance-postgres-reply").await;
}
