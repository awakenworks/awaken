// @formal evidence session_work.release_requires_exact_owner_and_epoch
// @formal evidence work.exact_boundary_reclaim
// @formal evidence work.pre_heartbeat_worker_identity
// @formal evidence work.single_active_claim
// @formal evidence work.terminal_lease_clear
//! Trait-generic **conformance suite** for the `WorkQueue` port — the shared behavioural
//! contract EVERY backend must satisfy, run here against the in-memory reference,
//! SQLite, and PostgreSQL backends (ADR-0059 / ADR-0039-slice-2.6 pattern, mirroring
//! `awaken-store-conformance`).
//!
//! Why this exists and what it drives: the Phase-1 `proptest` properties fuzz the *one*
//! in-memory backend. This suite LIFTS the same invariants to the PORT: it asserts the
//! durable backends are observably identical to the reference on the safety-critical
//! semantics — the single-active-lease cap, reclaim-at-TTL,
//! environment isolation, stop-frees-next, and remove_env-purges. A divergence here is a
//! real parity bug (the kind that ships a run twice on one deployment mode but not
//! another); making it a checked contract forces the backends to agree by construction.
//! Postgres joins the same suite behind its DB harness (`pg_tests.sh`).
//!
//! The suite itself lives in `awaken_store_conformance::work_queue` so a closed
//! backend can run the identical checks; this file only supplies the backends.

use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, SessionWorkStore, SessionWorkerLeaseStore, WorkQueries,
};
use awaken_session_types::SessionId;
use awaken_store_conformance::work_queue::run_suite;
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};
use std::sync::Arc;

fn block<F: std::future::Future>(f: F) -> F::Output {
    // The SQLite backend's pool admission is a timed wait, so the runtime needs
    // its timer and I/O drivers.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(
        async |clock| InMemoryWorkQueue::new(clock),
        LEASE_TTL_MS,
    ));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(
        async |clock| {
            SqliteWorkQueue::open_in_memory(clock)
                .await
                .expect("sqlite in-memory queue")
        },
        LEASE_TTL_MS,
    ));
}

#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

#[path = "work_queue_conformance/lease_acceptance.rs"]
mod lease_acceptance;

/// Fixture cause/effect table: F1 absent URL -> canonical visible skip;
/// F2 configured bad URL or schema failure -> hard failure; F3 live URL ->
/// each backend instance owns a unique schema and the unchanged generic
/// WorkQueue rules run against it. Only those exact schemas are dropped.
#[tokio::test]
async fn postgres_backend_conforms() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixtures = tokio::sync::Mutex::new(Vec::new());
    run_suite(
        async |clock| {
            let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 16).await;
            let store = PostgresWorkQueue::with_pool(fixture.pool.clone(), clock)
                .await
                .expect("queue schema migrations");
            fixtures.lock().await.push(fixture);
            store
        },
        LEASE_TTL_MS,
    )
    .await;
    for fixture in fixtures.into_inner() {
        fixture.cleanup().await;
    }
}

#[test]
fn fixture_absence_and_configured_failure_are_distinct() {
    // Child rule table against this exact Work backend: R1 optional+missing
    // => visible skip; R2 required+missing => failure; R3 required+empty or
    // unreachable => failure, never skip; R4 required+live => backend success
    // when the gate supplies its URL. The shared testkit resets both inherited
    // inputs before each child, so parent gate configuration cannot change R1.
    awaken_reliability_testkit::assert_postgres_fixture_child_contract("postgres_backend_conforms");
}

#[tokio::test]
async fn work_fixture_cleanup_preserves_another_live_schema() {
    // F3 two concurrent fixture owners -> separate schema identities. Dropping
    // the first must leave the second's actual Work row readable.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let first = work_postgres_fixture::WorkPostgresFixture::new(&url, 16).await;
    let second = work_postgres_fixture::WorkPostgresFixture::new(&url, 16).await;
    assert_ne!(first.scoped_url, second.scoped_url, "F3 different schemas");
    let first_store = PostgresWorkQueue::with_pool(first.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let second_store =
        PostgresWorkQueue::with_pool(second.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap();
    first_store.enqueue_healthcheck("first").await.unwrap();
    let second_id = second_store.enqueue_healthcheck("second").await.unwrap();
    drop(first_store);
    first.cleanup().await;
    assert!(
        second_store
            .get("second", &second_id)
            .await
            .unwrap()
            .is_some(),
        "F3 second row survives first cleanup"
    );
    drop(second_store);
    second.cleanup().await;
}

#[tokio::test]
async fn postgres_session_claim_is_independent_of_another_environments_writer() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 4).await;
    let store = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let session = SessionId::from("runnable-session");
    store.enqueue_healthcheck("blocked").await.unwrap();
    store.enqueue_session("runnable", &session).await.unwrap();

    let mut writer = fixture.pool.begin().await.unwrap();
    // Holding a row-level write in a different Environment must not stall an
    // existing Session's claim. The old table lock conflicts with this writer.
    sqlx::query("UPDATE work_queue_item SET metadata_json = '{}' WHERE environment_id = 'blocked'")
        .execute(&mut *writer)
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.acquire_session("runnable", &session, "worker"),
    )
    .await;
    writer.rollback().await.unwrap();
    drop(store);
    fixture.cleanup().await;

    let grant = result
        .expect("an unrelated Environment writer must not block Session Work")
        .unwrap()
        .unwrap();
    assert_eq!(grant.owner(), "worker");
}

async fn assert_renewal_does_not_take_queued_session(
    store: &(impl SessionWorkStore + SessionWorkerLeaseStore + WorkQueries),
    clock: &awaken_lease_testkit::ManualClock,
) {
    let session = SessionId::from("handoff");
    let env = "handoff";
    let original = store
        .acquire_session(env, &session, "old")
        .await
        .unwrap()
        .unwrap();
    assert!(store.release_session(&original).await.unwrap());
    let work_id = store.wake_session(env, &session).await.unwrap();
    assert_eq!(work_id, original.work_id());
    assert!(
        store
            .renew_session(env, &session, "old")
            .await
            .unwrap()
            .is_none()
    );
    let successor = store
        .acquire_session(env, &session, "new")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(successor.owner(), "new");
    assert!(successor.epoch() > original.epoch());

    clock.set(1_000);
    let foreign = store
        .renew_session(env, &session, "old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(foreign, successor);
    assert!(
        store
            .renew_exact_session(&original)
            .await
            .unwrap()
            .is_none()
    );
    let renewed = store
        .renew_exact_session(&successor)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renewed.epoch(), successor.epoch());
    assert!(renewed.expires_at_unix_ms() > successor.expires_at_unix_ms());

    clock.set(renewed.expires_at_unix_ms());
    assert!(store.renew_exact_session(&renewed).await.unwrap().is_none());
    assert!(
        store
            .renew_session(env, &session, "new")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .renew_session("missing", &session, "old")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.list("missing").await.unwrap().is_empty());
}

#[tokio::test]
async fn local_session_work_renewal_does_not_take_queued_work() {
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = InMemoryWorkQueue::new(clock.clone());
    assert_renewal_does_not_take_queued_session(&store, &clock).await;
    clock.set(0);
    let store = SqliteWorkQueue::open_in_memory(clock.clone())
        .await
        .unwrap();
    assert_renewal_does_not_take_queued_session(&store, &clock).await;
}

#[tokio::test]
async fn postgres_session_work_renewal_does_not_take_queued_work() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 4).await;
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = PostgresWorkQueue::with_pool(fixture.pool.clone(), clock.clone())
        .await
        .unwrap();
    assert_renewal_does_not_take_queued_session(&store, &clock).await;
    drop(store);
    fixture.cleanup().await;
}
