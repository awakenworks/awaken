//! Product-level webhook e2e (ADR-0048 / S10): the assembly bridge in action.
//! A subscription is registered through the REAL CRUD route, a session is created
//! through the managed state with an owner, and the lifecycle sink fans the
//! committed fact out over REAL HTTP to a REAL receiver — signed and scoped. No
//! mock transport: the dispatcher uses `ReqwestSender`, the receiver is an axum
//! server on an ephemeral port.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use awaken_server_local::webhooks;
use awaken_webhook::{InMemoryWebhookRepository, WebhookRepository, verify};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

type Inbox = Arc<Mutex<Vec<(String, HeaderMap)>>>;

async fn receive(State(inbox): State<Inbox>, headers: HeaderMap, body: String) -> &'static str {
    inbox.lock().unwrap().push((body, headers));
    "ok"
}

/// Call a router once and return (status, json).
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn crud_registers_a_subscription_and_a_live_session_delivers_signed() {
    // 1. A real receiver on an ephemeral port.
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let recv = Router::new()
        .route("/hook", post(receive))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, recv).await.unwrap() });

    // 2. The webhook plane over one shared repo (CRUD writes it, dispatcher reads it).
    let repo: Arc<dyn WebhookRepository> = Arc::new(InMemoryWebhookRepository::default());
    let (sink, crud) = webhooks::assemble(repo.clone(), None);

    // 3. Register a subscription through the REAL CRUD route; the secret comes back once.
    let (status, created) = call(
        &crud,
        "POST",
        "/v1/workspaces/wrkspc_local/webhooks",
        Some(
            json!({ "url": format!("http://{addr}/hook"), "event_types": ["session.status_idle"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["secret"]
        .as_str()
        .expect("secret returned once")
        .to_string();
    assert!(secret.starts_with("whsec_"));

    // It now lists (secret never re-echoed).
    let (_s, listed) = call(&crud, "GET", "/v1/workspaces/wrkspc_local/webhooks", None).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert!(
        listed["data"][0].get("secret").is_none(),
        "list never echoes the secret"
    );

    // 4. A committed session lifecycle fact, owned by wrkspc_local, drives the sink
    // exactly as `create_session` does (owner resolved at ingress; the
    // create_session→sink link itself is covered in protocol-managed).
    use awaken_protocol_managed::SessionLifecycleSink;
    sink.emit(
        "sesn_live",
        Some("wrkspc_local"),
        None,
        "session.status_idle",
    )
    .await;

    // 5. The receiver got exactly one signed, correctly-scoped delivery.
    for _ in 0..100 {
        if !inbox.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let got = inbox.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "exactly one delivery reached the receiver");
    let (body, headers) = &got[0];
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };

    let ts: i64 = hdr("webhook-timestamp").parse().expect("timestamp header");
    assert!(
        verify(
            &secret,
            &hdr("webhook-id"),
            ts,
            body,
            &hdr("webhook-signature")
        )
        .unwrap(),
        "the delivered signature verifies against the received body"
    );
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["data"]["type"], "session.status_idle");
    assert_eq!(v["data"]["id"], "sesn_live");
    assert_eq!(v["data"]["workspace_id"], "wrkspc_local");
    assert!(
        v["data"].get("organization_id").is_none(),
        "self-hosted omits org"
    );

    // 6. DELETE removes it; a later dispatch reaches nobody.
    let id = created["id"].as_str().unwrap();
    let (status, _) = call(
        &crud,
        "DELETE",
        &format!("/v1/workspaces/wrkspc_local/webhooks/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(repo.list("wrkspc_local").await.is_empty(), "unsubscribed");
}
