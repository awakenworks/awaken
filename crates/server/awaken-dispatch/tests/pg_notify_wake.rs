//! Real-Postgres e2e for `PgNotifyWake` (B-P2). Gated on `AWAKEN_TEST_DATABASE_URL`,
//! which is the one spelling `scripts/ci/pg_tests.sh` exports. It read
//! `AWAKEN_TEST_PG_URL` instead, so the gate set one variable and this file read
//! another: it printed `skip:` on every run this repository has ever made, and the
//! Postgres gate stayed green having never once reached this code.

use std::time::Duration;

use awaken_dispatch_contract::WakeSignal;
use awaken_dispatch::PgNotifyWake;
use sqlx::postgres::PgPoolOptions;

fn pg_url() -> Option<String> {
    std::env::var("AWAKEN_TEST_DATABASE_URL").ok()
}

/// One bounded delivery observation for both initial connection and reconnect.
/// C1 LISTEN/local waiter may still be registering; C2 hints are lossy and
/// duplicate-safe; C3 the real waiter completes or the whole budget expires.
/// R1 C1+C2 -> republish until C3(completion); R2 C3(timeout/panic) -> false.
/// The deadline also bounds pool acquisition/publication and aborts a retained
/// waiter on timeout, so a timed-out fixture cannot leave its listener alive.
async fn publish_until_woken(
    publisher: &dyn WakeSignal,
    mut wait_task: tokio::task::JoinHandle<()>,
    budget: Duration,
) -> bool {
    // awaken-allow(wait): settle on delivered wake, never a sleep or LISTEN query.
    let outcome = tokio::time::timeout(budget, async {
        loop {
            publisher.publish().await.expect("publish wake hint");
            if let Ok(joined) =
                tokio::time::timeout(Duration::from_millis(100), &mut wait_task).await
            {
                return joined.is_ok();
            }
        }
    })
    .await;
    if outcome.is_err() {
        wait_task.abort();
        let _ = wait_task.await;
    }
    outcome.unwrap_or(false)
}

#[tokio::test(start_paused = true)]
async fn delivery_observation_requires_completion_and_reaps_a_timed_out_waiter() {
    // Delivery-observation decision table: C1 successful publisher; C2 waiter
    // returns/panics/remains pending. R1 C1+return -> true; R2 C1+panic -> false;
    // R3 C1+pending -> false. E1 every waiter drops, including cancellation.
    // Reuse the existing local WakeSignal adapter to isolate this fixture
    // contract; the PostgreSQL cases below separately prove real transport.
    struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    for mode in 0..3 {
        let publisher = awaken_dispatch_execution::wake::LocalWakeSignal::new();
        let waiter = publisher.clone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let task = tokio::spawn(async move {
            let _guard = guard;
            match mode {
                0 => waiter.wait().await,
                1 => panic!("fixture waiter failed"),
                _ => std::future::pending().await,
            }
        });
        assert_eq!(
            publish_until_woken(&publisher, task, Duration::from_secs(1)).await,
            mode == 0,
            "R{}",
            mode + 1
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "E1");
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

    // R1 separate publisher/waiter connections + eventual listener/local
    // registration -> a delivered wake within the bound. A hint before either
    // registration may be lost; repeated hints must still reach this waiter.
    let wait_task = tokio::spawn(async move { waiter.wait().await });
    assert!(
        publish_until_woken(&publisher, wait_task, Duration::from_secs(5)).await,
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

    // R2 a delivered wake before the fault establishes the real hint path;
    // a visible LISTEN statement alone cannot establish that precondition.
    {
        let probe = PgNotifyWake::new(pool.clone(), channel);
        let probe_task = tokio::spawn(async move { probe.wait().await });
        assert!(
            publish_until_woken(&publisher, probe_task, Duration::from_secs(5)).await,
            "the listener delivers a wake before the drop"
        );
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
    let wait_task = tokio::spawn(async move { waiter.wait().await });
    let republisher = PgNotifyWake::new(pool.clone(), channel);
    // R3 terminated listener + reconnect + duplicate-safe hints -> delivered
    // wake; a permanently broken listener still fails at the common deadline.
    assert!(
        publish_until_woken(&republisher, wait_task, Duration::from_secs(20)).await,
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
    for explicit in [false, true] {
        let wake = std::sync::Arc::new(PgNotifyWake::new(pool.clone(), "awaken_wake_lifecycle"));
        // C1 the listener acquires the only pool connection; C2 a peer Arc
        // remains; C3 last owner shuts down/drops. R1 C1+C2 => connection
        // remains held; R2 C1+C3 => acquisition succeeds. The acquisition
        // probe avoids a stale pg_stat_activity LISTEN statement.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(Duration::from_millis(20), pool.acquire()).await {
                Err(_) => break,
                Ok(Ok(connection)) => drop(connection),
                Ok(Err(error)) => panic!("acquire lifecycle pool: {error}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the listener did not hold the one-connection pool"
            );
        }
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
}
