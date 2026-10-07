//! The one PostgreSQL checkout policy (`postgres` feature): every adapter's
//! pool is built from `postgres_pool_options`, so a transaction-poisoned or
//! dead connection is retired before another borrower sees it.

/// Shared PostgreSQL checkout policy. Compositions select capacity; every
/// adapter receives a clone of that pool rather than another transport owner.
#[cfg(feature = "postgres")]
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
    // SQLx 0.8.6 can cancel BEGIN before increasing its client-side depth.
    // Its ping drains ReadyForQuery but accepts a server transaction left open.
    // PostgreSQL permits transaction control in a top-level DO only outside
    // an explicit transaction block. This simple-protocol, no-data statement
    // therefore proves both transport liveness and clean server state in one
    // round trip. An open/aborted transaction rejects it; SQLx retires that
    // connection before another borrower can execute against the stale prefix.
    // Fresh connections run the same check so missing plpgsql USAGE fails at
    // connection construction rather than silently causing repeated reconnects.
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        sqlx::Executor::execute(
            connection,
            "DO $awaken_pool$ BEGIN ROLLBACK; END $awaken_pool$",
        ),
    )
    .await
    {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(sqlx::Error::PoolTimedOut),
    }
}

#[cfg(all(test, feature = "postgres"))]
mod postgres_tests;
