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
        async |clock| InMemoryWorkQueue::with_clock(clock),
        LEASE_TTL_MS,
    ));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(
        async |clock| SqliteWorkQueue::open_in_memory(clock).expect("sqlite in-memory queue"),
        LEASE_TTL_MS,
    ));
}

/// Fixture decision table: absent URL => explicit local skip; supplied URL plus
/// connection/schema failure => fail; supplied reachable URL => every shared
/// rule runs with fresh tables. Each schema has a generated unique name, so
/// simultaneous test processes cannot erase each other's authority or data.
#[tokio::test]
async fn postgres_backend_conforms() {
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        eprintln!("[skip] postgres_backend_conforms: AWAKEN_TEST_DATABASE_URL absent");
        return;
    };
    let admin = PgPool::connect(&url)
        .await
        .expect("explicit PostgreSQL URL must connect");
    let prefix = format!(
        "work_conformance_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let sequence = AtomicUsize::new(0);
    run_suite(
        async |clock| {
            let index = sequence.fetch_add(1, Ordering::Relaxed);
            let schema = format!("{prefix}_{index}");
            admin
                .execute(format!("CREATE SCHEMA {schema}").as_str())
                .await
                .expect("isolated schema");
            let pool = PgPoolOptions::new()
                .after_connect(move |connection, _| {
                    let statement = format!("SET search_path = {schema}");
                    Box::pin(async move {
                        connection.execute(statement.as_str()).await?;
                        Ok(())
                    })
                })
                .connect(&url)
                .await
                .expect("schema pool");
            PostgresWorkQueue::with_pool(pool, clock)
                .await
                .expect("queue schema migrations")
        },
        LEASE_TTL_MS,
    )
    .await;
    for index in 0..sequence.load(Ordering::Relaxed) {
        admin
            .execute(format!("DROP SCHEMA {prefix}_{index} CASCADE").as_str())
            .await
            .expect("remove owned schema");
    }
    admin.close().await;
}
