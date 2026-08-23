use awaken_run_ingress::{MemoryDispatchStore, PostgresDispatchStore, SqliteDispatchStore};
use awaken_run_ingress_testkit::assert_dispatch_operational_feed_conformance;
use awaken_run_ingress_testkit::{
    ConformanceCapabilities, assert_atomic_report_continuation_conformance,
    assert_dispatch_conformance, assert_session_reply_activity_rotation_conformance,
};

mod harness;

#[tokio::test]
async fn memory_dispatch_conforms() {
    // Coverage rationale. Causes: the canonical conformance matrix executes
    // against the in-memory backend. Effects: every shared dispatch, atomic
    // continuation, and operational-feed rule must pass. Constraint/Invariant:
    // Memory has no backend-only behavior. Decision rule: bind matrix row M1 to
    // the three authoritative helpers rather than duplicating their cases here.
    let store = MemoryDispatchStore::new();
    assert_dispatch_conformance(
        &store,
        "conformance-memory",
        ConformanceCapabilities::LOCAL_STORE,
    )
    .await;
    assert_atomic_report_continuation_conformance(&store, "conformance-memory-report").await;
    assert_dispatch_operational_feed_conformance(&store, "conformance-memory").await;
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
    let store = SqliteDispatchStore::open_in_memory().expect("open sqlite conformance store");
    assert_dispatch_conformance(
        &store,
        "conformance-sqlite",
        ConformanceCapabilities::LOCAL_STORE,
    )
    .await;
    assert_atomic_report_continuation_conformance(&store, "conformance-sqlite-report").await;
    assert_dispatch_operational_feed_conformance(&store, "conformance-sqlite").await;
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
