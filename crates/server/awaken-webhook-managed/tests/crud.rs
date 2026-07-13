//! Webhook-subscription CRUD over the config-plane router. This crate shipped with
//! zero tests; these pin the security-relevant arms: the tenant self-fence (404,
//! never an ownership disclosure), secret sealing (minted once, never echoed), and
//! the seal-failure / missing-url faults.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{WebhookEndpointDef, WebhookStore};
use awaken_credential_vault::{CredentialError, SecretRef, SecretStore};
use awaken_protocol_managed::{SessionLifecycleSink, WorkspaceScope};
use awaken_webhook::{ResolvedSubscription, SubscriptionSource, WebhookDispatcher, WebhookSender};
use awaken_webhook_managed::{
    ConfigPlaneSubscriptionSource, WebhookLifecycleSink, webhook_config_router,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct MemStore(Mutex<HashMap<String, WebhookEndpointDef>>);

impl WebhookStore for MemStore {
    fn put(&self, def: WebhookEndpointDef) {
        self.0.lock().unwrap().insert(def.id.clone(), def);
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.0.lock().unwrap().get(id).cloned()
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        self.0
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.workspace_id == workspace_id)
            .cloned()
            .collect()
    }
    fn delete(&self, id: &str) -> bool {
        self.0.lock().unwrap().remove(id).is_some()
    }
}

/// An in-memory secret vault. `failing` makes every `put` return a storage fault,
/// to exercise the seal-failure arm.
struct MemSecrets {
    map: Mutex<HashMap<String, String>>,
    failing: bool,
}

impl MemSecrets {
    fn ok() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            failing: false,
        }
    }
    fn failing() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            failing: true,
        }
    }
}

#[async_trait]
impl SecretStore for MemSecrets {
    async fn put(&self, r: &SecretRef, secret: RedactedString) -> Result<(), CredentialError> {
        if self.failing {
            return Err(CredentialError::Storage("seal failed".into()));
        }
        self.map
            .lock()
            .unwrap()
            .insert(r.0.clone(), secret.expose_secret().to_string());
        Ok(())
    }
    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.map
            .lock()
            .unwrap()
            .get(&r.0)
            .map(|s| RedactedString::new(s.clone()))
            .ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))
    }
}

fn seed(store: &MemStore, id: &str, ws: &str) {
    store.put(WebhookEndpointDef {
        id: id.into(),
        workspace_id: ws.into(),
        url: "https://old.example/hook".into(),
        event_types: vec!["run.completed".into()],
        disabled: false,
        secret_ref: SecretRef(format!("whsec:{id}")),
    });
}

/// Drive one request through the real router, stamping the tenant scope as the
/// management plane's `WorkspaceScope` extension would.
async fn call(
    store: Arc<MemStore>,
    secrets: Arc<MemSecrets>,
    method: &str,
    uri: &str,
    ws: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let app = webhook_config_router(
        store as Arc<dyn WebhookStore>,
        secrets as Arc<dyn SecretStore>,
    );
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    if let Some(ws) = ws {
        req.extensions_mut().insert(WorkspaceScope(ws.to_string()));
    }
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn create_mints_a_secret_seals_it_and_stores_the_row() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    let (status, body) = call(
        store.clone(),
        secrets.clone(),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://x.example/y", "event_types": ["run.completed"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["id"], "wh1");
    assert_eq!(body["workspace_id"], "ws_a");
    let secret = body["secret"]
        .as_str()
        .expect("plaintext secret returned once");
    assert!(!secret.is_empty());
    // The secret-free row is stored, sealed under its own ref.
    let row = store.get("wh1").expect("row stored");
    assert_eq!(row.workspace_id, "ws_a");
    assert_eq!(
        secrets.get(&row.secret_ref).await.unwrap().expose_secret(),
        secret,
        "the sealed secret matches the one returned"
    );
}

#[tokio::test]
async fn create_without_url_is_400() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    let (status, _) = call(
        store.clone(),
        secrets,
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "event_types": ["run.completed"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(store.get("wh1").is_none(), "no row on a bad request");
}

#[tokio::test]
async fn an_ssrf_shaped_url_is_rejected_and_seals_no_secret() {
    // The dispatcher fetches the endpoint server-side, so a private/metadata/non-https
    // URL is rejected at admission: 400, no row stored, and (critically) no secret
    // minted or sealed for it.
    for bad in [
        "https://169.254.169.254/latest/meta-data/", // cloud metadata (link-local)
        "https://127.0.0.1/admin",                   // loopback
        "http://hooks.example.com/x",                // non-https
    ] {
        let store = Arc::new(MemStore::default());
        let secrets = Arc::new(MemSecrets::ok());
        let (status, _) = call(
            store.clone(),
            secrets.clone(),
            "PUT",
            "/v1/config/webhook-subscriptions/wh_ssrf",
            Some("ws_a"),
            Some(json!({ "url": bad, "event_types": ["run.completed"] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} must be rejected");
        assert!(store.get("wh_ssrf").is_none(), "no row stored for {bad}");
        assert!(
            secrets.map.lock().unwrap().is_empty(),
            "no secret sealed for a rejected url {bad}"
        );
    }
}

#[tokio::test]
async fn seal_failure_is_500_and_stores_no_row() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::failing());
    let (status, _) = call(
        store.clone(),
        secrets,
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://x.example/y" })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        store.get("wh1").is_none(),
        "a failed seal leaves no dangling row"
    );
}

#[tokio::test]
async fn cross_tenant_put_is_404_and_does_not_hijack_the_row() {
    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_b");
    let (status, _) = call(
        store.clone(),
        Arc::new(MemSecrets::ok()),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://attacker.example/steal" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let row = store.get("wh1").unwrap();
    assert_eq!(row.workspace_id, "ws_b", "owner unchanged");
    assert_eq!(row.url, "https://old.example/hook", "url unchanged");
}

#[tokio::test]
async fn cross_tenant_get_is_404() {
    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_b");
    let (status, _) = call(
        store,
        Arc::new(MemSecrets::ok()),
        "GET",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cross_tenant_delete_is_a_noop() {
    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_b");
    let (status, _) = call(
        store.clone(),
        Arc::new(MemSecrets::ok()),
        "DELETE",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        store.get("wh1").is_some(),
        "another tenant's row is not deleted"
    );
}

#[tokio::test]
async fn update_in_place_preserves_the_sealed_secret() {
    let store = Arc::new(MemStore::default());
    store.put(WebhookEndpointDef {
        id: "wh1".into(),
        workspace_id: "ws_a".into(),
        url: "https://old.example/hook".into(),
        event_types: vec![],
        disabled: true,
        secret_ref: SecretRef("whsec:original".into()),
    });
    let (status, body) = call(
        store.clone(),
        Arc::new(MemSecrets::ok()),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://new.example/hook" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["url"], "https://new.example/hook");
    // No new secret minted on update, and none echoed.
    assert!(body.get("secret").is_none(), "update never echoes a secret");
    let row = store.get("wh1").unwrap();
    assert_eq!(
        row.secret_ref.0, "whsec:original",
        "sealed secret preserved"
    );
    assert!(row.disabled, "disabled flag preserved across an update");
}

#[tokio::test]
async fn get_projects_the_row_without_the_secret() {
    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_a");
    let (status, body) = call(
        store,
        Arc::new(MemSecrets::ok()),
        "GET",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["url"], "https://old.example/hook");
    assert!(
        body.get("secret").is_none(),
        "the view never leaks the secret"
    );
    assert!(body.get("secret_ref").is_none(), "nor the secret ref");
}

#[tokio::test]
async fn list_is_scoped_to_the_tenant() {
    let store = Arc::new(MemStore::default());
    seed(&store, "wh_a", "ws_a");
    seed(&store, "wh_b", "ws_b");
    let (status, body) = call(
        store,
        Arc::new(MemSecrets::ok()),
        "GET",
        "/v1/config/webhook-subscriptions",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "only the tenant's own subscriptions");
    assert_eq!(data[0]["id"], "wh_a");
}

// --- ConfigPlaneSubscriptionSource: the dispatcher's read port ---

/// Seed an enabled row AND seal its secret, so `matching` can resolve it.
async fn seed_resolvable(
    store: &MemStore,
    secrets: &MemSecrets,
    id: &str,
    ws: &str,
    types: &[&str],
) {
    let secret_ref = SecretRef(format!("whsec:{id}"));
    secrets
        .put(
            &secret_ref,
            RedactedString::new("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"),
        )
        .await
        .unwrap();
    store.put(WebhookEndpointDef {
        id: id.into(),
        workspace_id: ws.into(),
        url: "https://x.example/hook".into(),
        event_types: types.iter().map(|s| s.to_string()).collect(),
        disabled: false,
        secret_ref,
    });
}

fn source(store: Arc<MemStore>, secrets: Arc<MemSecrets>) -> ConfigPlaneSubscriptionSource {
    ConfigPlaneSubscriptionSource::new(
        store as Arc<dyn WebhookStore>,
        secrets as Arc<dyn SecretStore>,
    )
}

#[tokio::test]
async fn matching_resolves_an_enabled_subscription_with_its_secret() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh1", "ws_a", &["run.completed"]).await;
    let out = source(store, secrets)
        .matching("ws_a", "run.completed")
        .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, "wh1");
    assert!(
        !out[0].secret.is_empty(),
        "the secret is materialized at delivery"
    );
}

#[tokio::test]
async fn matching_skips_a_subscription_whose_secret_is_unresolvable() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    // Row present, but its secret was never sealed → unsignable → skipped.
    seed(&store, "wh1", "ws_a");
    let out = source(store, secrets)
        .matching("ws_a", "run.completed")
        .await;
    assert!(
        out.is_empty(),
        "an endpoint that can never sign is not delivered to"
    );
}

#[tokio::test]
async fn matching_skips_disabled_and_type_mismatched_subscriptions() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh_ok", "ws_a", &["run.completed"]).await;
    seed_resolvable(&store, &secrets, "wh_other", "ws_a", &["run.failed"]).await; // type mismatch
    seed_resolvable(&store, &secrets, "wh_dis", "ws_a", &["run.completed"]).await;
    let mut disabled = store.get("wh_dis").unwrap();
    disabled.disabled = true;
    store.put(disabled);

    let out = source(store, secrets)
        .matching("ws_a", "run.completed")
        .await;
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["wh_ok"],
        "only the enabled, type-matching endpoint"
    );
}

#[tokio::test]
async fn disable_flips_the_row_in_the_store() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh1", "ws_a", &[]).await;
    source(store.clone(), secrets).disable("wh1").await;
    assert!(
        store.get("wh1").unwrap().disabled,
        "auto-disable is a config-plane write"
    );
}

// --- WebhookLifecycleSink: the no-owner early return ---

struct CountingSource(Arc<Mutex<u32>>);
#[async_trait]
impl SubscriptionSource for CountingSource {
    async fn matching(&self, _ws: &str, _event: &str) -> Vec<ResolvedSubscription> {
        *self.0.lock().unwrap() += 1;
        Vec::new()
    }
    async fn disable(&self, _id: &str) {}
}

struct NoopSender;
#[async_trait]
impl WebhookSender for NoopSender {
    async fn post(&self, _u: &str, _h: Vec<(String, String)>, _b: String) -> Result<u16, String> {
        Ok(200)
    }
}

#[tokio::test]
async fn emit_without_a_workspace_owner_does_not_fan_out() {
    let calls = Arc::new(Mutex::new(0u32));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(CountingSource(calls.clone())),
        Arc::new(NoopSender),
    ));
    let sink = WebhookLifecycleSink::new(dispatcher, None);
    // No owner → the sink returns before spawning any dispatch (deterministic:
    // no owner means no spawn, so the source is never consulted).
    sink.emit("sesn_1", None, "session.created").await;
    assert_eq!(*calls.lock().unwrap(), 0, "no owner → no fan-out");
}
