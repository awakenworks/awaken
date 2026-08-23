//! Product-level webhook e2e (ADR-0048 / S10): the assembly bridge in action.
//! A subscription is registered through the REAL CRUD route, a session is created
//! through the managed state with an owner, and the lifecycle sink fans the
//! committed fact out over REAL HTTP to a REAL receiver — signed and scoped. No
//! mock transport: the dispatcher uses `ReqwestSender`, the receiver is an axum
//! server on an ephemeral port.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use awaken_config_resolver::InMemoryWebhookStore;
use awaken_coordinator::{install_managed_lifecycle_delivery, webhooks};
use awaken_credential_vault::InMemorySecretStore;
use awaken_protocol_managed::ManagedState;
use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;
use awaken_session_store::SqliteManagedSessionRepository;
use awaken_tenancy::WorkspaceScope;
use awaken_webhook::verify;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

type Inbox = Arc<Mutex<Vec<(String, HeaderMap)>>>;

async fn receive(State(inbox): State<Inbox>, headers: HeaderMap, body: String) -> &'static str {
    inbox.lock().unwrap().push((body, headers));
    "ok"
}

/// Stamp the owning workspace the way the guarded edge (`stamp_workspace_scope`)
/// would — this test drives the CRUD router directly, without the workspace-path
/// addressing + guard layers that normally publish the scope.
async fn stamp_local(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut()
        .insert(WorkspaceScope("wrkspc_local".to_string()));
    next.run(req).await
}

/// A second tenant, to prove cross-tenant access to a webhook id is fenced.
async fn stamp_intruder(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut()
        .insert(WorkspaceScope("wrkspc_intruder".to_string()));
    next.run(req).await
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
    // Causes: the fixtures below establish `crud registers a subscription and a live session
    // delivers signed` with the concrete inputs, state, dependencies, and failure triggers used by
    // this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 owning tenant authors a loopback subscription; C2
    // the canonical Coordinator lifecycle installer owns the Session notifier;
    // C3 Session creation commits and wakes its durable fact; C4 signing material
    // resolves; C5 the owner deletes the subscription; C6 service cancellation
    // follows delivery. Effects: E1 one scoped, signed HTTP event; E2 secret never
    // reappears in reads; E3 the endpoint row is absent; E4 the sole outbox loop
    // joins.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effects |
    // |---|---|---|---|---|---|---|---|
    // | W1 | yes | yes | yes | yes | no | no | E1,E2 |
    // | W2 | yes | any | any | any | yes | no | E3 |
    // | W3 | any | yes | any | any | any | yes | E4 |
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    // 1. A real receiver on an ephemeral port.
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let recv = Router::new()
        .route("/hook", post(receive))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, recv).await.unwrap() });

    // 2. The webhook plane over the config-plane stores (CRUD writes the endpoint row
    // + seals the secret; the dispatcher reads the same store + secrets). Stamp the
    // workspace onto the CRUD router as the guarded edge would.
    let store = Arc::new(InMemoryWebhookStore::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let sessions = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let state =
        Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()).with_session_repo(sessions));
    // Only the endpoint transport/admission policy differs in this loopback E2E;
    // lifecycle ownership remains the one Coordinator installation path.
    let delivery = webhooks::loopback_lifecycle_delivery(store.clone(), secrets.clone(), None);
    install_managed_lifecycle_delivery(&state, Some(delivery), &service_lifecycle)
        .expect("W1 bind the sole lifecycle notifier before traffic");
    let crud = webhooks::webhook_config_router_loopback(store, secrets);
    let crud = crud.layer(axum::middleware::from_fn(stamp_local));

    // 3. Register a subscription through the REAL CRUD route; the secret comes back once.
    let (status, created) = call(
        &crud,
        "PUT",
        "/v1/config/webhook-subscriptions/wh_1",
        Some(
            json!({ "url": format!("http://{addr}/hook"), "event_types": ["session.status_idled"] }),
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
    let (_s, listed) = call(&crud, "GET", "/v1/config/webhook-subscriptions", None).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert!(
        listed["data"][0].get("secret").is_none(),
        "list never echoes the secret"
    );

    // 4. The Session application transaction is the fact authority. Its bound
    // notifier carries no payload and only accelerates the supervised replay.
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder",
                "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
            }))
            .unwrap(),
            Some("wrkspc_local".into()),
        )
        .await
        .expect("W1 create Session and commit lifecycle fact");

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
    assert_eq!(v["data"]["type"], "session.status_idled");
    assert_eq!(v["data"]["id"], session.id);
    assert_eq!(v["data"]["workspace_id"], "wrkspc_local");
    assert!(
        v["data"].get("organization_id").is_none(),
        "self-hosted omits org"
    );

    // 6. DELETE removes it; the list is then empty (a later dispatch reaches nobody).
    let id = created["id"].as_str().unwrap();
    let (status, _) = call(
        &crud,
        "DELETE",
        &format!("/v1/config/webhook-subscriptions/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_s, listed) = call(&crud, "GET", "/v1/config/webhook-subscriptions", None).await;
    assert!(
        listed["data"].as_array().unwrap().is_empty(),
        "unsubscribed"
    );
    service_lifecycle
        .shutdown(std::time::Duration::from_secs(1))
        .await
        .expect("W3/E4 supervised outbox joins");
}

/// The webhook row carries its owner, so the handlers self-fence: another tenant's
/// id is a 404 on GET and a no-op on DELETE (idempotent, no ownership disclosure) —
/// the isolation that holds even under standalone, which has no ownership middleware.
#[tokio::test]
async fn cross_tenant_access_to_a_webhook_id_is_fenced() {
    // Causes: the fixtures below establish `cross tenant access to a webhook id` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `is fenced` and every asserted state transition or side effect
    // must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect decision rule W4: C1 an owner creates a scoped row and C2 an
    // intruder addresses that id -> E1 GET/list disclose nothing and E2 DELETE
    // is a no-op; C3 the owner reads again -> E3 its authoritative row survives.
    let store = Arc::new(InMemoryWebhookStore::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let crud = webhooks::webhook_config_router(store, secrets);
    let owner = crud.clone().layer(axum::middleware::from_fn(stamp_local));
    let intruder = crud.layer(axum::middleware::from_fn(stamp_intruder));

    // The owner creates wh_1.
    let (status, _) = call(
        &owner,
        "PUT",
        "/v1/config/webhook-subscriptions/wh_1",
        Some(json!({ "url": "https://example/hook" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The intruder cannot see it, cannot list it, and its DELETE is a silent no-op.
    let (status, _) = call(
        &intruder,
        "GET",
        "/v1/config/webhook-subscriptions/wh_1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant GET is 404");
    let (_s, listed) = call(&intruder, "GET", "/v1/config/webhook-subscriptions", None).await;
    assert!(
        listed["data"].as_array().unwrap().is_empty(),
        "intruder's list is empty"
    );
    let (status, _) = call(
        &intruder,
        "DELETE",
        "/v1/config/webhook-subscriptions/wh_1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "DELETE is idempotent");

    // The owner still has it — the intruder's DELETE touched nothing.
    let (status, _) = call(&owner, "GET", "/v1/config/webhook-subscriptions/wh_1", None).await;
    assert_eq!(status, StatusCode::OK, "W4/E3 owner's row survived");
}
