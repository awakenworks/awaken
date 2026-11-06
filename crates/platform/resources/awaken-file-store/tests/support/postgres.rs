//! The one PostgreSQL fixture for FileStore unit and integration contracts.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::time::Duration;

pub async fn isolated_postgres_url(tag: &'static str) -> Option<String> {
    // Cause/effect rules G1-G3: absent configuration permits a local skip;
    // explicit malformed/unreachable configuration and failed schema creation
    // are errors. This helper is exercised by the same live PG contract in
    // both test binaries. The negative gate probe sets a refused loopback URL
    // and must get a failed test, never a successful early return.
    // G4 distinct live test processes/tags => disjoint owned schemas; use the
    // original URL with an encoded search_path option, preserving its parameters.
    // G5 a lossy options projection could erase the schema => verify the actual
    // connection namespace before any catalog/migration write, never use public.
    // The runner owns the disposable database and its final cleanup.
    let base = awaken_reliability_testkit::postgres_fixture_url()?;
    assert!(tag.starts_with("t_file_") && tag.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    let schema = format!("{tag}_{}", std::process::id());
    let options: PgConnectOptions = base.parse().expect("configured PostgreSQL URL is valid");
    let admin = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .expect("configured PostgreSQL test database must be reachable");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin)
        .await
        .expect("reset the exact owned test schema");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .expect("create isolated test schema");
    admin.close().await;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options%5Bsearch_path%5D={schema}");
    let scoped = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("isolated PostgreSQL connection must succeed");
    let actual: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(&scoped)
        .await
        .expect("observe the actual test namespace");
    assert_eq!(
        actual, schema,
        "G5 never write through an unscoped connection"
    );
    scoped.close().await;
    Some(url)
}
