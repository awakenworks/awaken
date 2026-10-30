//! Explicit PostgreSQL fixtures and authority-clock observation for ingress tests.
// --- Live Postgres helpers, shared by the live suites -----------------------

use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// The explicitly admitted `AWAKEN_TEST_DATABASE_URL` for an active fixture.
pub fn database_url() -> String {
    awaken_reliability_testkit::postgres_fixture_url()
        .expect("PostgreSQL schema URL requires an admitted fixture")
}

/// A configured gate must fail; only an optional developer probe may skip.
pub fn postgres_connection_result<T>(
    result: Result<T, sqlx::Error>,
    required: bool,
    report_skip: impl FnOnce(&str),
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) if required => panic!("configured PostgreSQL test gate is unavailable: {error}"),
        Err(_) => {
            report_skip("[skip] optional development PostgreSQL is unavailable");
            None
        }
    }
}

pub async fn pool() -> Option<PgPool> {
    let url = awaken_reliability_testkit::postgres_fixture_url()?;
    postgres_connection_result(
        PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await,
        true,
        |message| println!("{message}"),
    )
}

/// The test database URL with `search_path` pinned to `schema`, for the one test
/// that exercises `connect()` (which opens its own pool, so it cannot use
/// `schema_pool`'s `after_connect` hook).
pub fn database_url_in_schema(schema: &str) -> String {
    let base = database_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20search_path%3D{schema}")
}

/// A pool isolated to a fresh, empty Postgres schema, so parallel tests do not
/// collide on the runtime's fixed table names. The production store takes no
/// table prefix (one runtime is one component); test isolation lives entirely in
/// the test, via `search_path` — it never leaks into the store's API. Returns
/// `None` for an absent optional fixture.
pub async fn schema_pool(schema: &'static str) -> Option<PgPool> {
    let admin = pool().await?;
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    admin.close().await;
    Some(
        PgPoolOptions::new()
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .expect("a reachable PostgreSQL fixture must open its isolated schema"),
    )
}

/// Observe the same moving clock that issues PostgreSQL fixture leases.
pub fn postgres_clock(pool: &PgPool) -> impl awaken_dispatch_testkit::ConformanceClock + use<> {
    let pool = pool.clone();
    awaken_dispatch_testkit::ObservedStoreClock::new(move || {
        let pool = pool.clone();
        async move { postgres_now_ms(&pool).await }
    })
}

pub async fn postgres_now_ms(pool: &PgPool) -> u64 {
    let now: i64 =
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(pool)
            .await
            .expect("configured PostgreSQL authority clock must answer");
    u64::try_from(now).expect("PostgreSQL authority clock is after the Unix epoch")
}

/// Seed immutable placement requirements that must reach the real Worker
/// compatibility check, rather than an epoch already pruned by the SQL query.
pub async fn seed_placement_candidate_prefix(
    pool: &PgPool,
    template: awaken_dispatch::RunDispatch,
    rows: i64,
    prefix: &str,
    priority: i64,
) {
    let template = serde_json::to_value(template).unwrap();
    sqlx::query(
        "INSERT INTO runtime_dispatch (run_id,thread_id,request,status,priority)
         SELECT $2 || g, $2 || 'thread-' || g,
          jsonb_set(jsonb_set($1::jsonb,'{activation,run_id}',to_jsonb($2 || g)),
            '{activation,thread_id}',to_jsonb($2 || 'thread-' || g)),
          'pending',$3 FROM generate_series(1,$4::bigint) AS g",
    )
    .bind(sqlx::types::Json(&template))
    .bind(prefix)
    .bind(priority)
    .bind(rows)
    .execute(pool)
    .await
    .unwrap();
}
