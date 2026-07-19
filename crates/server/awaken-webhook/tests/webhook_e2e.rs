//! End-to-end webhook delivery: a REAL HTTP receiver on an ephemeral port, an
//! in-memory resolved [`SubscriptionSource`] (persistence lives in the config
//! plane, out of this crate), and the production `ReqwestSender` — proving a
//! committed lifecycle fact is projected into a signed `webhook-*`-headed POST,
//! stamped with the owning `workspace_id`, that the receiver's Standard-Webhooks
//! verification accepts. No mocks in the transport.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_webhook::{
    ReqwestSender, ResolvedSubscription, SubscriptionSource, WebhookDispatcher, WebhookEvent,
    verify,
};
use axum::{Router, extract::State, http::HeaderMap, routing::post};
use tokio::io::AsyncReadExt;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // awaken-allow: secret (test sample key)

/// A one-subscription in-memory source (the real config-plane source is exercised
/// in awaken-webhook-managed / server-local e2e; here we isolate the transport).
struct OneSub(ResolvedSubscription);
#[async_trait::async_trait]
impl SubscriptionSource for OneSub {
    async fn matching(&self, _ws: &str, _event: &str) -> Vec<ResolvedSubscription> {
        vec![self.0.clone()]
    }
    async fn disable(&self, _id: &str) {}
}

/// What the receiver captured from a delivery.
#[derive(Clone, Default)]
struct Captured {
    body: String,
    webhook_id: String,
    webhook_timestamp: String,
    webhook_signature: String,
}

type Inbox = Arc<Mutex<Vec<Captured>>>;

async fn receive(State(inbox): State<Inbox>, headers: HeaderMap, body: String) -> &'static str {
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    inbox.lock().unwrap().push(Captured {
        body,
        webhook_id: hdr("webhook-id"),
        webhook_timestamp: hdr("webhook-timestamp"),
        webhook_signature: hdr("webhook-signature"),
    });
    "ok"
}

#[tokio::test]
async fn a_committed_fact_is_delivered_signed_and_scoped_to_a_real_receiver() {
    // 1. Stand up a real receiver on an ephemeral port.
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // 2. A workspace-scoped subscription pointing at the receiver, already resolved
    // (its secret materialized) — exactly what the config-plane source hands over.
    let source = Arc::new(OneSub(ResolvedSubscription {
        id: "wh_e2e".to_string(),
        url: format!("http://{addr}/hook"),
        secret: SECRET.to_string(),
    }));

    // 3. Project a committed fact — a session went idle, owned by wrkspc_acme.
    let event = WebhookEvent::new(
        "event_e2e_1",
        "2026-07-09T12:00:00Z",
        "session.status_idled",
        "sesn_e2e",
        "wrkspc_acme",
        None,
    );
    let ts = 1_752_000_000_i64;
    let dispatcher = WebhookDispatcher::new(source, Arc::new(ReqwestSender::default()));
    let report = dispatcher.dispatch(&event, ts).await;
    assert_eq!(
        report.delivered,
        vec!["wh_e2e".to_string()],
        "delivered to the real endpoint"
    );

    // 4. The receiver got exactly one signed, correctly-scoped delivery.
    // (Give the spawned server a beat to record, then assert.)
    for _ in 0..50 {
        if !inbox.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let captured = inbox.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "exactly one delivery reached the receiver"
    );
    let got = &captured[0];

    assert_eq!(got.webhook_id, "event_e2e_1");
    assert_eq!(got.webhook_timestamp, ts.to_string());

    // The receiver verifies the signature exactly as a Standard-Webhooks client would.
    assert!(
        verify(
            SECRET,
            &got.webhook_id,
            ts,
            &got.body,
            &got.webhook_signature
        )
        .unwrap(),
        "the delivered signature verifies against the received body"
    );

    // The payload carries the owning workspace and, self-hosted, no org.
    let v: serde_json::Value = serde_json::from_str(&got.body).unwrap();
    assert_eq!(v["type"], "event");
    assert_eq!(v["data"]["type"], "session.status_idled");
    assert_eq!(v["data"]["id"], "sesn_e2e");
    assert_eq!(v["data"]["workspace_id"], "wrkspc_acme");
    assert!(v["data"].get("organization_id").is_none());
}

fn event(id: &str) -> WebhookEvent {
    WebhookEvent::new(
        id,
        "2026-07-09T12:00:00Z",
        "session.status_idled",
        "sesn_e2e",
        "wrkspc_acme",
        None,
    )
}

async fn serve_status_sequence(
    statuses: Vec<axum::http::StatusCode>,
) -> (String, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let state = (Arc::new(statuses), attempts.clone());
    let app = Router::new()
        .route(
            "/hook",
            post(
                |State((statuses, attempts)): State<(
                    Arc<Vec<axum::http::StatusCode>>,
                    Arc<AtomicUsize>,
                )>| async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    statuses
                        .get(attempt)
                        .copied()
                        .unwrap_or(*statuses.last().expect("at least one status"))
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, attempts)
}

fn dispatcher(url: String, timeout: Duration, max_attempts: u32) -> WebhookDispatcher {
    WebhookDispatcher::new(
        Arc::new(OneSub(ResolvedSubscription {
            id: "wh_fault".into(),
            url,
            secret: SECRET.into(),
        })),
        Arc::new(ReqwestSender::with_timeout(timeout)),
    )
    .with_thresholds(max_attempts, 20)
}

#[tokio::test]
async fn real_http_429_is_retried_and_a_later_204_retires_the_delivery() {
    let (url, attempts) = serve_status_sequence(vec![
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        axum::http::StatusCode::NO_CONTENT,
    ])
    .await;
    let report = dispatcher(url, Duration::from_secs(1), 3)
        .dispatch(&event("event_429"), 1_752_000_000)
        .await;
    assert_eq!(report.delivered, vec!["wh_fault"]);
    assert!(report.failed.is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn real_http_500_exhausts_the_attempt_budget_and_stays_failed() {
    let (url, attempts) =
        serve_status_sequence(vec![axum::http::StatusCode::INTERNAL_SERVER_ERROR]).await;
    let report = dispatcher(url, Duration::from_secs(1), 3)
        .dispatch(&event("event_500"), 1_752_000_000)
        .await;
    assert!(report.delivered.is_empty());
    assert_eq!(report.failed, vec!["wh_fault"]);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_hung_receiver_is_bounded_by_the_attempt_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _socket = socket;
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
        }
    });
    let started = tokio::time::Instant::now();
    let report = dispatcher(url, Duration::from_millis(40), 2)
        .dispatch(&event("event_timeout"), 1_752_000_000)
        .await;
    assert_eq!(report.failed, vec!["wh_fault"]);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "two timed-out attempts must remain bounded"
    );
}

#[tokio::test]
async fn response_loss_retries_with_the_same_webhook_identity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_seen = seen.clone();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "request closed before its headers");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&bytes);
            let webhook_id = request
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("webhook-id")
                            .then(|| value.trim().to_string())
                    })
                })
                .expect("webhook-id header");
            server_seen.lock().unwrap().push(webhook_id);
            // Drop after accepting the request but before writing an HTTP
            // response: the sender cannot know whether the receiver committed.
        }
    });
    let report = dispatcher(url, Duration::from_secs(1), 2)
        .dispatch(&event("event_response_lost"), 1_752_000_000)
        .await;
    assert_eq!(report.failed, vec!["wh_fault"]);
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![
            "event_response_lost".to_string(),
            "event_response_lost".to_string()
        ]
    );
}
