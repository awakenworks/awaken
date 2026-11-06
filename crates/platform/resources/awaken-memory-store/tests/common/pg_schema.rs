//! One fresh, uniquely named PostgreSQL schema per test, and independent pools
//! on it. Shared by the Memory acceptance files that need two real clients.
#![allow(dead_code)] // Each integration binary uses a different subset.

use sqlx::Executor as _;
use sqlx::postgres::{PgConnectOptions, PgPool};

/// The configured fixture URL and a newly created schema, or `None` when the
/// fixture is not configured (the `*_fixture_admission` twin owns that rule).
pub async fn fresh_schema(tag: &str) -> Option<(String, String)> {
    let url = awaken_reliability_testkit::postgres_fixture_url()?;
    let admin = awaken_sql_storage::postgres_pool_options()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .expect("configured Memory PostgreSQL fixture must connect");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("{tag}_{}_{nonce:x}", std::process::id());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create the test schema");
    admin.close().await;
    Some((url, schema))
}

/// An independent pool whose sessions resolve to `schema`.
pub async fn connect(url: &str, schema: &str, application_name: &str) -> PgPool {
    let options = url
        .parse::<PgConnectOptions>()
        .expect("parse the configured Memory PostgreSQL fixture")
        .options([("search_path", schema)])
        .application_name(application_name);
    awaken_sql_storage::postgres_pool_options()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect_with(options)
        .await
        .expect("connect to the test schema")
}

/// Drop the schema this test created, and only that one.
pub async fn drop_schema(url: &str, schema: &str) {
    let admin = awaken_sql_storage::postgres_pool_options()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(url)
        .await
        .expect("connect to drop the test schema");
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop the test schema");
    admin.close().await;
}
