//! Live NATS wake-signal test (ADR-0019/ADR-0028). Only built under the `nats`
//! feature; skips when no NATS server is reachable, exactly as the Postgres live
//! suites skip without a database — so the default `cargo test` never needs NATS,
//! and `cargo test -p awaken-run-ingress --features nats` exercises it against a
//! real server (set `AWAKEN_TEST_NATS_URL`, default `nats://127.0.0.1:4222`).
#![cfg(feature = "nats")]

use std::time::Duration;

use awaken_run_ingress::{NatsWakeSignal, WakeSignal};

fn nats_url() -> String {
    std::env::var("AWAKEN_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string())
}

#[tokio::test]
async fn nats_wake_signal_notifies_a_waiter() {
    // Connect, or skip if no NATS is reachable.
    let subject = "awaken.test.wake";
    let waiter = match NatsWakeSignal::connect(&nats_url(), subject).await {
        Ok(w) => w,
        Err(err) => {
            println!("[skip] no NATS reachable: {err}");
            return;
        }
    };
    let publisher = NatsWakeSignal::connect(&nats_url(), subject)
        .await
        .expect("second connection");

    // Start waiting first (so the subscription is established), then publish a
    // hint from another node; the waiter must be woken.
    let wait = tokio::spawn(async move { waiter.wait().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    publisher.publish().await.expect("publish a wake hint");

    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the wake hint was delivered across the fleet")
        .expect("wait task");
}
