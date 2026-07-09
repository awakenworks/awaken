//! Real-Postgres e2e for `PgNotifyWake` (B-P2). Gated on `AWAKEN_TEST_PG_URL` —
//! skips cleanly when no database is provided, runs against a live one in CI/local.

use std::time::Duration;

use awaken_run_ingress::{PgNotifyWake, WakeSignal};
use sqlx::postgres::PgPoolOptions;

fn pg_url() -> Option<String> {
    std::env::var("AWAKEN_TEST_PG_URL").ok()
}

#[tokio::test]
async fn publish_wakes_a_waiter_on_another_connection() {
    let Some(url) = pg_url() else {
        eprintln!("skip: AWAKEN_TEST_PG_URL unset");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect pg");

    let waiter = PgNotifyWake::new(pool.clone(), "awaken_dispatch_wake");
    let publisher = PgNotifyWake::new(pool.clone(), "awaken_dispatch_wake");

    // Waiter parks; a publish on a *different* connection must wake it. The listener
    // needs a moment to establish its LISTEN before the notify fires.
    let wait_task = tokio::spawn(async move { waiter.wait().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    publisher.publish().await.expect("publish");

    let woke = tokio::time::timeout(Duration::from_secs(5), wait_task).await;
    assert!(woke.is_ok(), "PgNotifyWake.wait did not return after a cross-connection publish");
}

#[tokio::test]
async fn publish_is_ok_with_no_waiter() {
    let Some(url) = pg_url() else {
        eprintln!("skip: AWAKEN_TEST_PG_URL unset");
        return;
    };
    let pool = PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("connect pg");
    // A notify with nobody listening is a no-op, not an error (hint semantics).
    PgNotifyWake::new(pool, "awaken_dispatch_wake")
        .publish()
        .await
        .expect("publish with no waiter should be Ok");
}
