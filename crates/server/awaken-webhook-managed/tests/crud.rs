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
use awaken_protocol_managed::WorkspaceScope;
use awaken_webhook_managed::webhook_config_router;
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
    let secret = body["secret"].as_str().expect("plaintext secret returned once");
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
    assert!(store.get("wh1").is_some(), "another tenant's row is not deleted");
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
    assert_eq!(row.secret_ref.0, "whsec:original", "sealed secret preserved");
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
    assert!(body.get("secret").is_none(), "the view never leaks the secret");
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
