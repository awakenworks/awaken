use awaken_run_ingress::{MemoryDispatchStore, PostgresDispatchStore, SqliteDispatchStore};
use awaken_run_ingress_testkit::assert_dispatch_operational_feed_conformance;
use awaken_run_ingress_testkit::{ConformanceCapabilities, assert_dispatch_conformance};

mod harness;

#[tokio::test]
async fn memory_dispatch_conforms() {
    let store = MemoryDispatchStore::new();
    assert_dispatch_conformance(
        &store,
        "conformance-memory",
        ConformanceCapabilities::LOCAL_STORE,
    )
    .await;
    assert_dispatch_operational_feed_conformance(&store, "conformance-memory").await;
}

#[tokio::test]
async fn sqlite_dispatch_conforms() {
    let store = SqliteDispatchStore::open_in_memory().expect("open sqlite conformance store");
    assert_dispatch_conformance(
        &store,
        "conformance-sqlite",
        ConformanceCapabilities::LOCAL_STORE,
    )
    .await;
    assert_dispatch_operational_feed_conformance(&store, "conformance-sqlite").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_dispatch_conforms() {
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
    assert_dispatch_operational_feed_conformance(&store, "conformance-postgres").await;
}
