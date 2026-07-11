//! The full live path (ADR-0048 / S10, A#1): a REAL guarded `POST /v1/sessions`
//! fans a webhook out to a REAL receiver. The guard (the authz aspect) resolves
//! the owning workspace from the API key and publishes it; `stamp_workspace_scope`
//! maps it to the wire crate's `WorkspaceScope`; `create_session` hands it to the
//! lifecycle sink; the dispatcher signs and delivers over real HTTP. The core
//! session never stores tenancy — the owner is an edge value throughout.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use awaken_authz_enforce::{EnforceEngine, TokenSpec, guard};
use awaken_protocol_managed::ManagedState;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_host::{ManagedHost, SharedHost};
use awaken_server_local::webhooks;
use awaken_webhook::{
    InMemoryWebhookRepository, WebhookRepository, WebhookSubscription, generate_secret, verify,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Router, middleware};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A runtime model that is never invoked (session create only prepares + idles).
struct DeadModel;
#[async_trait::async_trait]
impl LlmExecutor for DeadModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        unreachable!("create_session never runs a turn")
    }
}

type Inbox = Arc<Mutex<Vec<(String, HeaderMap)>>>;

async fn receive(State(inbox): State<Inbox>, headers: HeaderMap, body: String) -> &'static str {
    inbox.lock().unwrap().push((body, headers));
    "ok"
}

#[tokio::test]
async fn a_guarded_live_session_delivers_a_signed_scoped_webhook() {
    // 1. Real receiver.
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let recv = Router::new()
        .route("/hook", post(receive))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, recv).await.unwrap() });

    // 2. Webhook plane + a subscription for the token's workspace.
    let repo = Arc::new(InMemoryWebhookRepository::default());
    let (sink, _crud) = webhooks::assemble(repo.clone(), None);
    let secret = generate_secret();
    repo.upsert(WebhookSubscription {
        id: "wh_live".to_string(),
        workspace_id: "wrkspc_test".to_string(),
        url: format!("http://{addr}/hook"),
        secret: secret.clone(),
        event_types: vec![
            "session.status_idled".to_string(),
            "session.status_terminated".to_string(),
        ],
        disabled: false,
    })
    .await;

    // 3. A managed surface with the sink, wrapped: guard (resolves + publishes the
    // owning workspace) → stamp_workspace_scope (maps it to WorkspaceScope).
    let host = Arc::new(SharedHost::new(Arc::new(DeadModel), "test"));
    let managed = Arc::new(ManagedState::new(ManagedHost::new(host)).with_lifecycle_sink(sink));
    let engine = Arc::new(EnforceEngine::seeded());
    let token = engine
        .mint(TokenSpec {
            token_id: "tok_app".into(),
            service_id: "app".into(),
            workspace_id: "wrkspc_test".into(),
            role: "admin".into(),
            expires_at: None,
        })
        .expect("mint token");
    let app = awaken_protocol_managed::router(managed)
        .layer(middleware::from_fn(webhooks::stamp_workspace_scope))
        .layer(middleware::from_fn_with_state(engine, guard));

    // 4. Create a session through the guarded HTTP surface with the token.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"agent":"assistant"}"#))
        .unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "guarded create succeeds");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let session: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();

    // 5. The receiver got the signed, workspace-scoped delivery for THIS session.
    for _ in 0..100 {
        if !inbox.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let got = inbox.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "exactly one delivery reached the receiver");
    let (payload, headers) = &got[0];
    let hdr = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let ts: i64 = hdr("webhook-timestamp").parse().unwrap();
    assert!(
        verify(
            &secret,
            &hdr("webhook-id"),
            ts,
            payload,
            &hdr("webhook-signature")
        )
        .unwrap(),
        "the delivered signature verifies"
    );
    let v: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(v["data"]["type"], "session.status_idled");
    assert_eq!(
        v["data"]["id"], session_id,
        "the fact is about the created session"
    );
    assert_eq!(
        v["data"]["workspace_id"], "wrkspc_test",
        "owner resolved from the API key by the guard, stamped edge-side"
    );

    // 6. Archive the SAME session through the guarded surface → a second signed,
    // workspace-scoped delivery carrying the terminal `session.status_terminated`
    // fact. The owner is resolved from the persisted session (the archive edge
    // carries only the id), so the same workspace subscription matches.
    let archive = Request::builder()
        .method("POST")
        .uri(format!("/v1/sessions/{session_id}/archive"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(archive).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "guarded archive succeeds");

    for _ in 0..100 {
        if inbox.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let got2 = inbox.lock().unwrap().clone();
    assert_eq!(
        got2.len(),
        2,
        "create idled + archive terminated = two deliveries"
    );
    let (payload2, headers2) = &got2[1];
    let hdr2 = |k: &str| {
        headers2
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let ts2: i64 = hdr2("webhook-timestamp").parse().unwrap();
    assert!(
        verify(
            &secret,
            &hdr2("webhook-id"),
            ts2,
            payload2,
            &hdr2("webhook-signature")
        )
        .unwrap(),
        "the terminated delivery's signature verifies too"
    );
    let v2: serde_json::Value = serde_json::from_str(payload2).unwrap();
    assert_eq!(v2["data"]["type"], "session.status_terminated");
    assert_eq!(
        v2["data"]["id"], session_id,
        "the terminal fact is about the same session"
    );
    assert_eq!(v2["data"]["workspace_id"], "wrkspc_test");
}
