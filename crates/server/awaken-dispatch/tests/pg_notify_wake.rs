//! Real-Postgres e2e for `PgNotifyWake` (B-P2). Gated on `AWAKEN_TEST_PG_URL` —
//! skips cleanly when no database is provided, runs against a live one in CI/local.

use std::time::Duration;

use awaken_dispatch::{PgNotifyWake, WakeSignal};
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

    // Waiter awaits; a publish on a *different* connection must wake it. The listener
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
    // waiter awaits, a cross-connection publish wakes it.
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

    // C1 this fixture's listener is connected; C2 its backend is terminated.
    // R1 reconnect restores its hints. Scope the fault to our exact LISTEN query:
    // unrelated listeners (including concurrent lifecycle tests) must survive.
    let terminated = sqlx::query_scalar::<_, bool>(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE query = $1 AND datname = current_database() AND pid <> pg_backend_pid()",
    )
    .bind(format!("LISTEN \"{channel}\""))
    .fetch_all(&pool)
    .await
    .expect("terminate the listener backends");
    assert!(
        terminated.iter().any(|terminated| *terminated),
        "fault must reach this fixture's listener"
    );

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

#[tokio::test]
async fn listener_ownership_releases_connection_on_shutdown_and_last_drop() {
    // C1 one-connection pool is pinned by LISTEN; C2 shared owner released;
    // C3 final owner shutdown/drop. R1 C1+C2 => listener remains alive;
    // R2 C1+C3 => connection becomes available without closing the shared pool.
    // R3 shutdown during pool acquisition cancels that wait and joins promptly.
    let Some(url) = pg_url() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let observer = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    for explicit in [false, true] {
        let wake = std::sync::Arc::new(PgNotifyWake::new(pool.clone(), "awaken_wake_lifecycle"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Pool num_idle=0 also occurs while a prior connection is being
                // returned. Observe the actual LISTEN, not that ambiguous count.
                let listening: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE query = $1 AND state = 'idle' AND datname = current_database()")
                    .bind("LISTEN \"awaken_wake_lifecycle\"")
                    .fetch_one(&observer).await.unwrap();
                if listening > 0 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let peer = wake.clone();
        drop(wake);
        assert!(
            std::sync::Arc::try_unwrap(peer.clone()).is_err(),
            "shared owner cannot consume shutdown"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pool.acquire())
                .await
                .is_err(),
            "R1"
        );
        let wake = match std::sync::Arc::try_unwrap(peer) {
            Ok(wake) => wake,
            Err(_) => panic!("last owner"),
        };
        if explicit {
            wake.shutdown().await;
        } else {
            drop(wake);
        }
        drop(
            tokio::time::timeout(Duration::from_secs(5), pool.acquire())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let held = pool.acquire().await.unwrap();
    let wake = PgNotifyWake::new(pool.clone(), "awaken_wake_acquisition");
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(1), wake.shutdown())
        .await
        .expect("R3");
    drop(held);
    pool.close().await;
    observer.close().await;
}
