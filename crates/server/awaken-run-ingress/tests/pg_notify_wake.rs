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
    assert!(
        woke.is_ok(),
        "PgNotifyWake.wait did not return after a cross-connection publish"
    );
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

/// The background listener RECONNECTS after its connection drops: if the listening
/// backend is terminated, the loop backs off and re-establishes its LISTEN, so a
/// later publish still wakes a waiter. A missed hint during the gap is covered by
/// the poll fallback; this proves the hint path self-heals rather than dying.
#[tokio::test]
async fn listener_reconnects_after_its_connection_is_dropped() {
    let Some(url) = pg_url() else {
        eprintln!("skip: AWAKEN_TEST_PG_URL unset");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect pg");
    // A unique channel isolates this test's LISTEN backends from other suites.
    let channel = "awaken_dispatch_wake_reconnect";
    let waiter = PgNotifyWake::new(pool.clone(), channel);
    let publisher = PgNotifyWake::new(pool.clone(), channel);

    // Let the listener establish its LISTEN, then prove the hint path is live: a
    // waiter parks, a cross-connection publish wakes it.
    tokio::time::sleep(Duration::from_millis(400)).await;
    {
        let probe = PgNotifyWake::new(pool.clone(), channel);
        let probe_task = tokio::spawn(async move { probe.wait().await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        publisher.publish().await.expect("first publish");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), probe_task)
                .await
                .is_ok(),
            "the listener delivers a wake before the drop"
        );
    }

    // Terminate every backend currently parked on a LISTEN — including this waiter's
    // listener connection — forcing its `recv` to error and the loop to reconnect.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE query ILIKE 'LISTEN%' AND pid <> pg_backend_pid()",
    )
    .execute(&pool)
    .await
    .expect("terminate the listener backends");

    // Give the loop time to notice the drop, back off (500ms), and re-LISTEN.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // A publish after the reconnect must still wake the waiter — the hint self-healed.
    let wait_task = tokio::spawn(async move { waiter.wait().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let republisher = PgNotifyWake::new(pool.clone(), channel);
    republisher
        .publish()
        .await
        .expect("publish after reconnect");
    let woke = tokio::time::timeout(Duration::from_secs(5), wait_task).await;
    assert!(
        woke.is_ok(),
        "the listener reconnected and delivered a wake after its connection dropped"
    );
}
