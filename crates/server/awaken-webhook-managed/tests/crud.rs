//! Webhook-subscription CRUD over the config-plane router. This crate shipped with
//! zero tests; these pin the security-relevant arms: the tenant self-fence (404,
//! never an ownership disclosure), secret sealing (minted once, never echoed), and
//! the seal-failure / missing-url faults.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{WebhookEndpointDef, WebhookOutboxEvent, WebhookStore};
use awaken_credential_vault::{CredentialError, SecretRef, SecretStore};
use awaken_session_contract::{
    ManagedSessionRepository, PersistedSession, SessionLifecycleFact, SessionLifecycleSink,
};
use awaken_tenancy::WorkspaceScope;
use awaken_webhook::{ResolvedSubscription, SubscriptionSource, WebhookDispatcher, WebhookSender};
use awaken_webhook_managed::{
    ConfigPlaneSubscriptionSource, WebhookLifecycleSink, webhook_config_router,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Extension, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct MemStore(
    Mutex<HashMap<String, WebhookEndpointDef>>,
    Mutex<HashMap<String, WebhookOutboxEvent>>,
);

#[derive(Default)]
struct SessionOutbox(Mutex<HashMap<String, SessionLifecycleFact>>);

#[async_trait]
impl ManagedSessionRepository for SessionOutbox {
    async fn save_owned(&self, _owner_scope: &str, _session: PersistedSession) {}
    async fn save_owned_with_lifecycle(
        &self,
        _owner_scope: &str,
        _session: PersistedSession,
        fact: SessionLifecycleFact,
    ) {
        self.append_lifecycle(fact).await;
    }
    async fn append_lifecycle(&self, fact: SessionLifecycleFact) {
        self.0
            .lock()
            .unwrap()
            .entry(fact.id.clone())
            .or_insert(fact);
    }
    async fn archive_with_lifecycle(
        &self,
        _session_id: &str,
        _archived_at: &str,
        fact: SessionLifecycleFact,
    ) {
        self.append_lifecycle(fact).await;
    }
    async fn delete_with_lifecycle(&self, _session_id: &str, fact: SessionLifecycleFact) {
        self.append_lifecycle(fact).await;
    }
    async fn pending_lifecycle(&self) -> Vec<SessionLifecycleFact> {
        self.0.lock().unwrap().values().cloned().collect()
    }
    async fn complete_lifecycle(&self, fact_id: &str) {
        self.0.lock().unwrap().remove(fact_id);
    }
    async fn get(&self, _session_id: &str) -> Option<PersistedSession> {
        None
    }
}

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
    fn enqueue_outbox(&self, event: WebhookOutboxEvent) -> bool {
        let mut rows = self.1.lock().unwrap();
        if rows.contains_key(&event.id) {
            return false;
        }
        rows.insert(event.id.clone(), event);
        true
    }
    fn pending_outbox(&self) -> Vec<WebhookOutboxEvent> {
        self.1.lock().unwrap().values().cloned().collect()
    }
    fn complete_outbox(&self, event_id: &str) -> bool {
        self.1.lock().unwrap().remove(event_id).is_some()
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
    async fn delete(&self, r: &SecretRef) -> Result<(), CredentialError> {
        self.map.lock().unwrap().remove(&r.0);
        Ok(())
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

/// A sender that reports the delivered body over a channel, so the out-of-band
/// (spawned) dispatch is observable deterministically.
struct RecordingSender(tokio::sync::mpsc::UnboundedSender<String>);
#[async_trait]
impl WebhookSender for RecordingSender {
    async fn post(&self, _u: &str, _h: Vec<(String, String)>, body: String) -> Result<u16, String> {
        let _ = self.0.send(body);
        Ok(200)
    }
}

/// A source that always yields one live, signable subscription.
struct OneSubSource(String);
#[async_trait]
impl SubscriptionSource for OneSubSource {
    async fn matching(&self, _ws: &str, _event: &str) -> Vec<ResolvedSubscription> {
        vec![ResolvedSubscription {
            id: "wh1".into(),
            url: "https://x.example/hook".into(),
            secret: self.0.clone(),
        }]
    }
    async fn disable(&self, _id: &str) {}
}

struct CountingStatusSender {
    calls: Arc<AtomicUsize>,
    status: Result<u16, String>,
}

struct RecoveringSender {
    calls: Arc<AtomicUsize>,
    failing: Arc<AtomicBool>,
}

#[async_trait]
impl WebhookSender for RecoveringSender {
    async fn post(&self, _u: &str, _h: Vec<(String, String)>, _b: String) -> Result<u16, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            Err("temporary outage".into())
        } else {
            Ok(200)
        }
    }
}

#[async_trait]
impl WebhookSender for CountingStatusSender {
    async fn post(&self, _u: &str, _h: Vec<(String, String)>, _b: String) -> Result<u16, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.status.clone()
    }
}

async fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("webhook attempts within the deadline");
}

#[tokio::test]
async fn a_stable_fact_id_is_enqueued_and_delivered_only_once_per_pending_row() {
    let store = Arc::new(MemStore::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));
    let sink =
        WebhookLifecycleSink::with_outbox(dispatcher, None, store.clone() as Arc<dyn WebhookStore>);

    sink.emit_fact(
        "session:sesn_1:created",
        "sesn_1",
        Some("ws_a"),
        "session.created",
    )
    .await;
    sink.emit_fact(
        "session:sesn_1:created",
        "sesn_1",
        Some("ws_a"),
        "session.created",
    )
    .await;
    wait_for_calls(&calls, 1).await;
    tokio::task::yield_now().await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        store.pending_outbox().is_empty(),
        "successful delivery retires the row"
    );
}

#[tokio::test]
async fn failed_delivery_keeps_the_stable_fact_pending_for_recovery() {
    let store = Arc::new(MemStore::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Err("injected outage".into()),
        }),
    ));
    let sink =
        WebhookLifecycleSink::with_outbox(dispatcher, None, store.clone() as Arc<dyn WebhookStore>);

    sink.emit_fact(
        "session:sesn_1:archived",
        "sesn_1",
        Some("ws_a"),
        "session.archived",
    )
    .await;
    wait_for_calls(&calls, 3).await;
    tokio::task::yield_now().await;

    let pending = store.pending_outbox();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "session:sesn_1:archived");
    assert_eq!(pending[0].object_id, "sesn_1");
}

#[tokio::test]
async fn rebuilding_the_sink_drains_rows_left_by_the_prior_process() {
    let store = Arc::new(MemStore::default());
    store.enqueue_outbox(WebhookOutboxEvent {
        id: "session:sesn_1:deleted".into(),
        created_at: "2026-07-19T00:00:00Z".into(),
        event_type: "session.deleted".into(),
        object_id: "sesn_1".into(),
        workspace_id: "ws_a".into(),
        organization_id: None,
        timestamp: 1_768_780_800,
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));

    let _rebuilt =
        WebhookLifecycleSink::with_outbox(dispatcher, None, store.clone() as Arc<dyn WebhookStore>);
    wait_for_calls(&calls, 1).await;
    tokio::task::yield_now().await;

    assert!(
        store.pending_outbox().is_empty(),
        "startup recovery retires a successfully redelivered row"
    );
}

#[tokio::test]
async fn session_local_outbox_is_drained_after_commit_before_notify_crash() {
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(SessionLifecycleFact {
            id: "session:sesn_tx:created".into(),
            session_id: "sesn_tx".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1_768_780_800,
        })
        .await;
    // No sink existed at commit time: this is the exact former crash window.
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));
    let _restarted = WebhookLifecycleSink::with_session_outbox(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
    );

    wait_for_calls(&calls, 1).await;
    assert!(outbox.pending_lifecycle().await.is_empty());
}

#[tokio::test]
async fn periodic_reconciliation_redelivers_without_restart_or_a_new_event() {
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(SessionLifecycleFact {
            id: "session:sesn_retry:created".into(),
            session_id: "sesn_retry".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1_768_780_800,
        })
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let failing = Arc::new(AtomicBool::new(true));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(RecoveringSender {
            calls: calls.clone(),
            failing: failing.clone(),
        }),
    ));
    let _sink = WebhookLifecycleSink::with_session_outbox_interval(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        std::time::Duration::from_millis(10),
    );
    wait_for_calls(&calls, 3).await;
    assert_eq!(outbox.pending_lifecycle().await.len(), 1);

    failing.store(false, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !outbox.pending_lifecycle().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("periodic reconciliation should redeliver after recovery");
}

#[tokio::test]
async fn emit_with_a_workspace_owner_fans_out_a_stamped_monotonic_event() {
    // The lifecycle→webhook bridge: a committed fact with an owner builds the
    // Anthropic-shaped event (session id, event type, workspace, org stamped) and
    // delivers it out-of-band. The event id is `event_<n>`, monotonic from zero.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(RecordingSender(tx)),
    ));
    let sink = WebhookLifecycleSink::new(dispatcher, Some("org_root".into()));

    sink.emit("sesn_1", Some("ws_a"), "session.status_idled")
        .await;
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("delivery within the deadline")
        .expect("a body was delivered");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["type"], "event");
    assert_eq!(v["id"], "event_0", "first event id counts from zero");
    assert_eq!(v["data"]["type"], "session.status_idled");
    assert_eq!(v["data"]["id"], "sesn_1", "the object id is the session");
    assert_eq!(v["data"]["workspace_id"], "ws_a");
    assert_eq!(v["data"]["organization_id"], "org_root", "org is stamped");

    // A second emit advances the monotonic sequence.
    sink.emit("sesn_2", Some("ws_a"), "session.status_terminated")
        .await;
    let body2 = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("second delivery")
        .expect("a second body");
    let v2: Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(v2["id"], "event_1", "the event id is monotonic");
    assert_eq!(v2["data"]["id"], "sesn_2");
}

#[tokio::test]
async fn stamp_workspace_scope_maps_resolved_tenancy_to_the_scope() {
    // The aspect→core seam: the guard-resolved `RequestTenancy` becomes the
    // handler-visible `WorkspaceScope`; a request with no resolved tenancy passes
    // through unstamped (the handler then falls back to the default scope).
    async fn probe(scope: Option<Extension<WorkspaceScope>>) -> String {
        scope
            .map(|Extension(WorkspaceScope(w))| w)
            .unwrap_or_else(|| "<none>".into())
    }
    let app = Router::new()
        .route("/p", get(probe))
        .layer(axum::middleware::from_fn(
            awaken_webhook_managed::stamp_workspace_scope,
        ));

    // Tenancy stamped upstream → mapped to WorkspaceScope("ws_x").
    let mut stamped = Request::builder()
        .method("GET")
        .uri("/p")
        .body(Body::empty())
        .unwrap();
    stamped
        .extensions_mut()
        .insert(awaken_authz_enforce::RequestTenancy {
            workspace_id: "ws_x".into(),
        });
    let resp = app.clone().oneshot(stamped).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ws_x", "resolved tenancy maps to the scope");

    // No tenancy → passes through unstamped; the handler sees no scope.
    let bare = Request::builder()
        .method("GET")
        .uri("/p")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(bare).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"<none>", "a bare request is not stamped");
}

#[tokio::test]
async fn an_unscoped_request_falls_back_to_the_default_scope() {
    // A flat / single-tenant deployment stamps no WorkspaceScope: create and read
    // both resolve to the seeded `default` scope, so the row is self-consistent.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    let (status, body) = call(
        store.clone(),
        secrets.clone(),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        None,
        Some(json!({ "url": "https://x.example/y" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["workspace_id"], "default", "unscoped → default scope");
    assert_eq!(store.get("wh1").unwrap().workspace_id, "default");

    // An equally-unscoped GET reads it back under the same default scope.
    let (gstatus, gbody) = call(
        store,
        secrets,
        "GET",
        "/v1/config/webhook-subscriptions/wh1",
        None,
        None,
    )
    .await;
    assert_eq!(gstatus, StatusCode::OK);
    assert_eq!(gbody["url"], "https://x.example/y");
}

#[tokio::test]
async fn an_ssrf_shaped_url_is_rejected_on_the_update_path_too() {
    // `put_subscription` runs `validate_url` BEFORE the create-vs-update branch, so an
    // SSRF-shaped URL is rejected at admission on the UPDATE path as well — only the
    // create path was covered. A tenant with an existing (safe) row must not be able
    // to repoint it at a loopback/metadata/non-https target: 400, and the stored row
    // is left untouched (no rewrite of url/event_types).
    for bad in [
        "https://169.254.169.254/latest/meta-data/", // cloud metadata (link-local)
        "https://127.0.0.1/admin",                   // loopback
        "http://hooks.example.com/x",                // non-https
    ] {
        let store = Arc::new(MemStore::default());
        seed(&store, "wh1", "ws_a"); // an existing, owned row at a safe url
        let (status, _) = call(
            store.clone(),
            Arc::new(MemSecrets::ok()),
            "PUT",
            "/v1/config/webhook-subscriptions/wh1",
            Some("ws_a"),
            Some(json!({ "url": bad, "event_types": ["run.failed"] })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} must be rejected on update"
        );
        let row = store.get("wh1").expect("the row still exists");
        assert_eq!(
            row.url, "https://old.example/hook",
            "a rejected update must not repoint the endpoint at {bad}"
        );
        assert_eq!(
            row.event_types,
            vec!["run.completed".to_string()],
            "nor rewrite its event types"
        );
    }
}

// --- assemble / assemble_loopback: the composition fns (the guarded-sender +
//     strict-policy pairing vs. the loopback pairing) ---

/// Drive one request through an arbitrary already-built router (the `assemble*`
/// fns hand back their own `Router`, so the shared `call` helper does not apply).
async fn drive(router: Router, method: &str, uri: &str, ws: &str, body: Value) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(WorkspaceScope(ws.to_string()));
    router.oneshot(req).await.unwrap().status()
}

/// `assemble` is the production composition: it pairs `ReqwestSender::guarded()`
/// with `strict_endpoint_url_policy()`. The guarded-sender + strict-policy pairing
/// is only covered downstream, so drive the CRUD router `assemble` returns and prove
/// the STRICT policy is wired — a loopback endpoint is rejected at admission (400).
/// Also confirm the returned lifecycle sink is a live `SessionLifecycleSink` (its
/// no-owner path is a deterministic no-op, needing no network).
#[tokio::test]
async fn assemble_wires_the_strict_ssrf_policy_and_returns_a_working_sink() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    let (sink, router) = awaken_webhook_managed::assemble(
        store.clone() as Arc<dyn WebhookStore>,
        secrets as Arc<dyn SecretStore>,
        Some("org_root".into()),
    );

    // Strict policy wired: a loopback endpoint is rejected, and no row is stored.
    let status = drive(
        router,
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        "ws_a",
        json!({ "url": "https://127.0.0.1/admin", "event_types": ["run.completed"] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "assemble must wire the strict SSRF policy"
    );
    assert!(
        store.get("wh1").is_none(),
        "a rejected create stores no row"
    );

    // The returned sink is a real lifecycle sink: the no-owner emit is a no-op.
    sink.emit("sesn_1", None, "session.created").await;
}

/// `assemble_loopback` is the e2e composition: `ReqwestSender::default()` paired
/// with an admit-everything policy. Prove the PERMISSIVE policy is wired — the same
/// `127.0.0.1` endpoint `assemble` rejects is ADMITTED here (201 Created), the exact
/// behavioural difference between the two composition fns. This also transitively
/// drives the private `assemble_with` both delegate to.
#[tokio::test]
async fn assemble_loopback_admits_a_loopback_endpoint_the_strict_path_rejects() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    let (_sink, router) = awaken_webhook_managed::assemble_loopback(
        store.clone() as Arc<dyn WebhookStore>,
        secrets as Arc<dyn SecretStore>,
        None,
    );

    let status = drive(
        router,
        "PUT",
        "/v1/config/webhook-subscriptions/wh_lb",
        "ws_a",
        json!({ "url": "https://127.0.0.1:9999/hook", "event_types": ["run.completed"] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the loopback composition admits its own 127.0.0.1 receiver"
    );
    assert_eq!(
        store.get("wh_lb").expect("the row is stored").workspace_id,
        "ws_a"
    );
}

#[tokio::test]
async fn delete_of_an_owned_row_removes_it() {
    // The happy-path unsubscribe: a tenant deleting its own subscription tears the
    // row out (the counterpart to the cross-tenant no-op).
    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_a");
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
        store.get("wh1").is_none(),
        "the tenant's own row is actually removed"
    );
}
