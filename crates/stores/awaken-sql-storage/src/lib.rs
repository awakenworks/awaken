//! SQL storage primitives every SQL store shares: the exact unsigned BIGINT codec and,
//! with the `postgres` feature, the PostgreSQL checkout policy.
//!
//! The exact unsigned BIGINT codec is driver-independent. Every SQL storage
//! context uses its one implementation and retains its own error projection.
//! Every PostgreSQL adapter's
//! pool is built from `postgres_pool_options`, so a transaction-poisoned or
//! dead connection is retired before another borrower sees it.

mod stored_u64;

pub use stored_u64::{StoredU64, StoredU64Error};

/// Shared PostgreSQL checkout policy. Compositions select capacity; every
/// adapter receives a clone of that pool rather than another transport owner.
#[cfg(feature = "postgres")]
#[expect(
    clippy::disallowed_methods,
    reason = "the one PostgreSQL checkout policy: the builder and its connection hooks (ADR-0087)"
)]
pub fn postgres_pool_options() -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .idle_timeout(std::time::Duration::from_secs(5))
        .test_before_acquire(false)
        .after_connect(|connection, _metadata| Box::pin(verify_postgres_checkout(connection)))
        .before_acquire(|connection, _metadata| {
            Box::pin(async move { verify_postgres_checkout(connection).await.map(|()| true) })
        })
}

#[cfg(feature = "postgres")]
async fn verify_postgres_checkout(
    connection: &mut sqlx::postgres::PgConnection,
) -> Result<(), sqlx::Error> {
    // Construction and recycled checkout share this unchanged health budget.
    // The probe owns SQL state/error semantics independently of this deadline.
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        probe_postgres_checkout_state(connection),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(sqlx::Error::PoolTimedOut),
    }
}

#[cfg(feature = "postgres")]
async fn probe_postgres_checkout_state(
    connection: &mut sqlx::postgres::PgConnection,
) -> Result<(), sqlx::Error> {
    // SQLx 0.8.6 can cancel BEGIN before increasing its client-side depth.
    // Its ping drains ReadyForQuery but accepts a server transaction left open.
    // PostgreSQL permits transaction control in a top-level DO only outside
    // an explicit transaction block. This simple-protocol, no-data statement
    // therefore proves both transport liveness and clean server state in one
    // round trip. An open/aborted transaction rejects it; SQLx retires that
    // connection before another borrower can execute against the stale prefix.
    // Fresh connections run the same check so missing plpgsql USAGE fails at
    // connection construction rather than silently causing repeated reconnects.
    sqlx::Executor::execute(
        connection,
        "DO $awaken_pool$ BEGIN ROLLBACK; END $awaken_pool$",
    )
    .await
    .map(|_| ())
}

#[cfg(all(test, feature = "postgres"))]
mod postgres_tests;
