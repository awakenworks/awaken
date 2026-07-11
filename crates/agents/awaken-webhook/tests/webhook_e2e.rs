//! End-to-end webhook delivery: a REAL HTTP receiver on an ephemeral port, an
//! in-memory resolved [`SubscriptionSource`] (persistence lives in the config
//! plane, out of this crate), and the production `ReqwestSender` — proving a
//! committed lifecycle fact is projected into a signed `webhook-*`-headed POST,
//! stamped with the owning `workspace_id`, that the receiver's Standard-Webhooks
//! verification accepts. No mocks in the transport.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use awaken_webhook::{
    ReqwestSender, ResolvedSubscription, SubscriptionSource, WebhookDispatcher, WebhookEvent,
    verify,
};
use axum::{Router, extract::State, http::HeaderMap, routing::post};

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
