//! Real-Postgres e2e for `PgNotifyWake` (B-P2). Gated on `AWAKEN_TEST_DATABASE_URL`,
//! which is the one spelling `scripts/ci/pg_tests.sh` exports. It read
//! `AWAKEN_TEST_PG_URL` instead, so the gate set one variable and this file read
//! another: it printed `skip:` on every run this repository has ever made, and the
//! Postgres gate stayed green having never once reached this code.

use std::time::Duration;

use awaken_dispatch::{PgNotifyWake, WakeSignal};
use sqlx::postgres::PgPoolOptions;

fn pg_url() -> Option<String> {
    std::env::var("AWAKEN_TEST_DATABASE_URL").ok()
}

/// Every backend whose last statement was this channel's LISTEN.
///
/// `btrim(query, \';\')` is load-bearing: the driver\'s first LISTEN is recorded
/// as `LISTEN "c"` and its reconnect as `LISTEN "c";`. An exact-equality match
/// therefore sees the first listener and never the re-established one, which is
/// exactly why this file reached for a fixed sleep after the fault instead of
/// asking whether the listener was back.
const LISTENING_BACKENDS: &str = "SELECT pid FROM pg_stat_activity \
     WHERE btrim(query, ';') = $1 AND datname = current_database()";

/// The pids of the backends currently holding `LISTEN "<channel>"`.
///
/// This is the durable fact every LISTEN-establishment wait in this file
/// settles on: the row exists once the backend has actually issued its LISTEN,
/// so a NOTIFY published after it is observed cannot be missed.
async fn listening_pids(observer: &sqlx::PgPool, channel: &str) -> Vec<i32> {
    sqlx::query_scalar::<_, i32>(LISTENING_BACKENDS)
        .bind(format!("LISTEN \"{channel}\""))
        .fetch_all(observer)
        .await
        .expect("read the listening backends")
}

/// Settle on a backend listening to `channel` that is not one of `excluded`.
///
/// Excluding the pids observed beforehand is what makes this *this* listener's
/// LISTEN rather than any listener's: a concurrent suite, or the backend this
/// test just terminated, cannot satisfy it.
///
/// A fixed sleep in this position guesses how long the listener task needs to
/// reach its LISTEN. The guess is the same whether the connection is instant or
/// the pool is contended, it names nothing, and when it is wrong the NOTIFY is
/// published into a channel nobody is listening to yet -- so the test fails as
/// "the wake was never delivered", blaming the product for the fixture.
async fn wait_for_listener(observer: &sqlx::PgPool, channel: &str, excluded: &[i32]) -> Vec<i32> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let pids = listening_pids(observer, channel).await;
        let established: Vec<i32> = pids
            .iter()
            .copied()
            .filter(|pid| !excluded.contains(pid))
            .collect();
        if !established.is_empty() {
            return established;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no backend established LISTEN \"{channel}\" outside {excluded:?}; \
             currently listening: {pids:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn publish_wakes_a_waiter_on_another_connection() {
    let Some(url) = pg_url() else {
        eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect pg");

    // A unique channel isolates this test's LISTEN backends from other suites,
    // so the established-listener fact below names this waiter and no other.
    let channel = "awaken_dispatch_wake_cross_connection";
    let waiter = PgNotifyWake::new(pool.clone(), channel);
    let publisher = PgNotifyWake::new(pool.clone(), channel);

    // Waiter awaits; a publish on a *different* connection must wake it. The
    // notify may only fire once this waiter's own LISTEN backend is committed
    // to pg_stat_activity.
    let before = listening_pids(&pool, channel).await;
    let wait_task = tokio::spawn(async move { waiter.wait().await });
    wait_for_listener(&pool, channel, &before).await;
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
        eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");
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
        eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");
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

    // Prove the hint path is live before the fault: a waiter awaits, and a
    // cross-connection publish wakes it once its LISTEN backend is committed.
    {
        let before = listening_pids(&pool, channel).await;
        let probe = PgNotifyWake::new(pool.clone(), channel);
        let mut probe_task = tokio::spawn(async move { probe.wait().await });
        wait_for_listener(&pool, channel, &before).await;
        // Republish on the same bounded-loop rule the reconnect arm below already
        // uses, and for the same reason. `wait_for_listener` proves the LISTEN
        // *statement* is visible in pg_stat_activity, which is not the same fact
        // as the backend being registered to receive: a NOTIFY landing in that
        // window is dropped, and a single publish then waits out the timeout for
        // a hint that no longer exists. Retrying costs nothing — a wake hint is
        // idempotent — and keeps the failure exact: a listener that never
        // delivers still fails here, saying the wake never arrived.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let woke = loop {
            publisher.publish().await.expect("first publish");
            match tokio::time::timeout(Duration::from_millis(100), &mut probe_task).await {
                Ok(joined) => break joined.is_ok(),
                Err(_) if tokio::time::Instant::now() < deadline => {}
                Err(_) => break false,
            }
        };
        assert!(woke, "the listener delivers a wake before the drop");
    }

    // C1 this fixture's listener is connected; C2 its backend is terminated.
    // R1 reconnect restores its hints. Scope the fault to our exact LISTEN query:
    // unrelated listeners (including concurrent lifecycle tests) must survive.
    let terminated = sqlx::query_as::<_, (i32, bool)>(
        "SELECT pid, pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE btrim(query, ';') = $1 AND datname = current_database() \
           AND pid <> pg_backend_pid()",
    )
    .bind(format!("LISTEN \"{channel}\""))
    .fetch_all(&pool)
    .await
    .expect("terminate the listener backends");
    assert!(
        terminated.iter().any(|(_, terminated)| *terminated),
        "fault must reach this fixture's listener"
    );
    // A publish after the reconnect must still wake the waiter — the hint
    // self-healed.
    //
    // The re-established LISTEN is deliberately *not* the anchor here. This
    // fixture's own pooled connections carry the same `LISTEN "<channel>"` as
    // their last statement after a listener releases them, so pg_stat_activity
    // cannot tell the reconnected backend from a returned one, and any pid- or
    // start-time rule built on it would be a guess wearing a fact's clothes.
    //
    // The durable fact this rule is actually about is the delivered wake. The
    // publish is a hint, and a hint dropped during the reconnect gap is by
    // design (the poll fallback stays authoritative), so re-publishing until
    // the waiter wakes is what the product promises. Bounding the whole loop
    // keeps the failure exact: a listener that never returns still fails here,
    // and it fails saying the wake never arrived rather than blaming a sleep
    // that was too short.
    let mut wait_task = tokio::spawn(async move { waiter.wait().await });
    let republisher = PgNotifyWake::new(pool.clone(), channel);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let woke = loop {
        republisher
            .publish()
            .await
            .expect("publish after reconnect");
        match tokio::time::timeout(Duration::from_millis(100), &mut wait_task).await {
            Ok(joined) => break joined.is_ok(),
            Err(_) if tokio::time::Instant::now() < deadline => {}
            Err(_) => break false,
        }
    };
    assert!(
        woke,
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
        // Pool num_idle=0 also occurs while a prior connection is being
        // returned. Observe the actual LISTEN, not that ambiguous count.
        wait_for_listener(&observer, "awaken_wake_lifecycle", &[]).await;
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
