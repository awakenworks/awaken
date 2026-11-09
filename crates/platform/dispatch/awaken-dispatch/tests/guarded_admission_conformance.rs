// @formal evidence session_execution.one_physical_attempt_per_thread
//! The guarded physical-admission decision table lives in
//! `awaken_store_conformance::physical_admission`; this file supplies each
//! Dispatch backend with the Registry it is paired with in production
//! (Memory/Memory, SQLite/SQLite, PostgreSQL/PostgreSQL on one process pool).

use std::sync::Arc;

use awaken_dispatch::{
    MemoryDispatchStore, PlacementRequirements, PostgresDispatchStore, RunClaim, RunDispatch,
    SqliteDispatchStore, WorkerSnapshot,
};
use awaken_dispatch_contract::{DispatchClaims, DispatchEnqueue};
use awaken_dispatch_testkit::worker_http::activation;
use awaken_lease_testkit::ManualClock;
use awaken_store_conformance::physical_admission::assert_guarded_admission_follows_standing;
use awaken_time::WallClockExt;
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};

async fn claim_with<S: DispatchEnqueue + DispatchClaims + ?Sized>(
    store: &S,
    tag: String,
    worker: WorkerSnapshot,
) -> RunClaim {
    let request = RunDispatch::new(activation(&tag))
        .with_placement(PlacementRequirements::remote_required())
        .with_worker_codec_requirements();
    let run_id = request.run_id().clone();
    store.enqueue(request).await.unwrap();
    RunClaim::from(
        &store
            .claim_run_compatible(&run_id, &worker, 60_000)
            .await
            .unwrap()
            .expect("a Ready incarnation claims its Run")
            .lease,
    )
}

#[tokio::test]
async fn memory_guarded_admission_follows_incarnation_standing() {
    let clock = Arc::new(ManualClock::new(10_000_000));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let directory = MemoryWorkerDirectory::new();
    assert_guarded_admission_follows_standing(
        "memory",
        store.as_ref(),
        &directory,
        || {
            let clock = clock.clone();
            async move { clock.now_ms() }
        },
        |tag, worker| {
            let store = store.clone();
            async move { claim_with(store.as_ref(), tag, worker).await }
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_guarded_admission_follows_incarnation_standing() {
    let clock = Arc::new(ManualClock::new(10_000_000));
    let store = Arc::new(
        SqliteDispatchStore::open_in_memory(clock.clone())
            .await
            .unwrap(),
    );
    let temporary = tempfile::tempdir().unwrap();
    let directory = SqliteWorkerDirectory::open(temporary.path().join("worker-registry.db"))
        .await
        .unwrap();
    assert_guarded_admission_follows_standing(
        "sqlite",
        store.as_ref(),
        &directory,
        || {
            let clock = clock.clone();
            async move { clock.now_ms() }
        },
        |tag, worker| {
            let store = store.clone();
            async move { claim_with(store.as_ref(), tag, worker).await }
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_shared_file_guarded_admission_follows_incarnation_standing() {
    // The embedded product composition opens both native owners on one file.
    // Their original Registry exclusion must be the Dispatch writer, rather
    // than blocking a second BEGIN IMMEDIATE on that same database.
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("shared-process.db");
    let clock = Arc::new(ManualClock::new(10_000_000));
    let store = Arc::new(
        SqliteDispatchStore::open(database.to_str().unwrap(), clock.clone())
            .await
            .unwrap(),
    );
    let directory = SqliteWorkerDirectory::open(&database).await.unwrap();
    let transcript = assert_guarded_admission_follows_standing(
        "sqlite-shared-file",
        store.as_ref(),
        &directory,
        || {
            let clock = clock.clone();
            async move { clock.now_ms() }
        },
        |tag, worker| {
            let store = store.clone();
            async move { claim_with(store.as_ref(), tag, worker).await }
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), transcript)
        .await
        .expect("shared-file native admission must not wait on its own Registry transaction");
}

#[tokio::test]
async fn postgres_guarded_admission_follows_incarnation_standing() {
    // The production pairing: Registry and Dispatch on one process pool, the
    // Registry transaction carried into the admission. Database time is the
    // store clock. An absent fixture is reported by the fixture, never passed.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    let directory = PostgresWorkerDirectory::with_pool(pool.clone())
        .await
        .unwrap();
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    assert_guarded_admission_follows_standing(
        "postgres",
        store.as_ref(),
        &directory,
        || {
            let pool = pool.clone();
            async move {
                let now: i64 = sqlx::query_scalar(
                    "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                u64::try_from(now).unwrap()
            }
        },
        |tag, worker| {
            let store = store.clone();
            async move { claim_with(store.as_ref(), tag, worker).await }
        },
    )
    .await;
    pool.close().await;
}
