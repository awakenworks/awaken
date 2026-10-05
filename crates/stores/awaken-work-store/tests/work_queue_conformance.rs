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

use awaken_session_work_contract::WorkQueue;
use awaken_store_conformance::work_queue::run_suite;
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(
        awaken_time::system_clock(),
        async |clock| InMemoryWorkQueue::new(clock),
        LEASE_TTL_MS,
    ));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(
        awaken_time::system_clock(),
        async |clock| SqliteWorkQueue::open_in_memory(clock).expect("sqlite in-memory queue"),
        LEASE_TTL_MS,
    ));
}

#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

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
        awaken_time::system_clock(),
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
    // F1 no URL -> visible skip without a fallback DB; F2 explicit empty or
    // unreachable URL -> failed backend test, never a passing skip. The actual
    // backend case runs in a subprocess, avoiding process-global env races.
    let run = |url: Option<&str>| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["postgres_backend_conforms", "--exact", "--nocapture"]);
        child.env_remove("AWAKEN_TEST_DATABASE_URL");
        if let Some(url) = url {
            child.env("AWAKEN_TEST_DATABASE_URL", url);
        }
        child.output().expect("Work fixture rule child")
    };
    let absent = run(None);
    assert!(absent.status.success(), "F1");
    assert!(
        String::from_utf8_lossy(&absent.stderr).contains("SKIPPED PostgreSQL fixture"),
        "F1 visible skip"
    );
    for url in ["", "postgres://127.0.0.1:1/unreachable"] {
        let bad = run(Some(url));
        assert!(!bad.status.success(), "F2 configured failure");
        assert!(
            !String::from_utf8_lossy(&bad.stderr).contains("SKIPPED PostgreSQL fixture"),
            "F2 no false skip"
        );
    }
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
