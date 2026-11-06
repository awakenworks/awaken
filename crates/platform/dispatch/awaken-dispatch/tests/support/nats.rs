//! Live NATS wake signal. Optional absence is explicit; a configured or required
//! broker must connect and deliver a real message. The native-service CI owner
//! sets AWAKEN_TEST_NATS_URL and AWAKEN_TEST_REQUIRE_NATS=1.
#![cfg(feature = "nats")]

use std::time::Duration;

use super::NatsWakeSignal;
use awaken_dispatch_contract::WakeSignal;

// async-nats 0.50 flush() acknowledges a local socket write, not server
// processing. A self-published probe on the SAME connection is ordered after
// its SUB commands. Receiving it proves those commands reached the broker.
async fn confirm_server_subscription(client: &async_nats::Client, context: &str) {
    use futures_util::StreamExt;
    let subject = client.new_inbox();
    let mut probe = client
        .subscribe(subject.clone())
        .await
        .expect("probe subscription");
    client
        .publish(subject.clone(), b"subscription-ready".to_vec().into())
        .await
        .expect("publish subscription probe");
    let message = tokio::time::timeout(Duration::from_secs(5), probe.next())
        .await
        .unwrap_or_else(|_| panic!("{context}: broker subscription barrier deadline"))
        .expect("broker subscription probe");
    assert_eq!(message.subject.as_str(), subject);
    assert_eq!(message.payload.as_ref(), b"subscription-ready");
}

#[test]
fn nats_fixture_absence_and_configured_failure_are_distinct() {
    awaken_reliability_testkit::assert_nats_fixture_child_contract(
        "wake::nats_tests::nats_wake_signal_notifies_a_waiter",
    );
}

#[tokio::test]
async fn nats_wake_signal_notifies_a_waiter() {
    let Some(url) = awaken_reliability_testkit::nats_fixture_url() else {
        return;
    };
    let subject = format!("awaken.test.wake.{}", std::process::id());
    let waiter = tokio::time::timeout(
        Duration::from_secs(5),
        NatsWakeSignal::connect(&url, subject.clone()),
    )
    .await
    .expect("configured NATS fixture must answer within its deadline")
    .expect("configured NATS fixture must connect");
    let publisher = tokio::time::timeout(
        Duration::from_secs(5),
        NatsWakeSignal::connect(&url, subject.clone()),
    )
    .await
    .expect("publisher connect deadline")
    .expect("publisher connection");
    // Observe an actual message independently of WakeSignal::wait's intentional
    // polling fallback. A completed task alone cannot establish broker delivery.
    let mut observed = tokio::time::timeout(Duration::from_secs(5), async {
        let client = async_nats::connect(&url).await.expect("delivery observer");
        let subscription = client
            .subscribe(subject.clone())
            .await
            .expect("observer subscription");
        confirm_server_subscription(&client, "observer").await;
        (client, subscription)
    })
    .await
    .expect("observer readiness deadline");
    // Poll the fresh wait once so its SUB command is queued, then establish
    // broker processing on that same connection before another client publishes.
    // The probe has its own subject and cannot satisfy the actual wake wait.
    let mut wait = Box::pin(tokio::task::unconstrained(waiter.wait()));
    assert!(matches!(
        futures_util::poll!(&mut wait),
        std::task::Poll::Pending
    ));
    confirm_server_subscription(&waiter.client, "waiter").await;
    publisher.publish().await.expect("publish one wake hint");
    tokio::time::timeout(Duration::from_secs(5), &mut wait)
        .await
        .expect("the published wake hint must release the waiter");
    use futures_util::StreamExt;
    let message = tokio::time::timeout(Duration::from_secs(5), observed.1.next())
        .await
        .expect("delivery deadline")
        .expect("an actual NATS message");
    assert_eq!(message.subject.as_str(), subject);
    assert!(
        message.payload.is_empty(),
        "wake hints carry no durable authority"
    );
    awaken_reliability_testkit::report_nats_fixture_exercised();
}
