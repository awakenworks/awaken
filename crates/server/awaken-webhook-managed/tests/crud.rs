//! Webhook-subscription CRUD over the config-plane router. This crate shipped with
//! zero tests; these pin the security-relevant arms: the tenant self-fence (404,
//! never an ownership disclosure), secret sealing (minted once, never echoed), and
//! the seal-failure / missing-url faults.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{
    InMemoryWebhookStore, WebhookAuthoringPatch, WebhookAuthoringState, WebhookDeliveryOutcome,
    WebhookDeliveryState, WebhookEndpointDef, WebhookMutationIntent, WebhookStore,
};
use awaken_credential_vault::{CredentialError, SecretRef, SecretStore};
use awaken_session_contract::{
    LifecycleFactDelivery, LifecycleFactNotifier, ManagedLifecycleFact, ManagedSessionRepository,
    PersistedSession,
};
use awaken_tenancy::WorkspaceScope;
use awaken_webhook::{
    ResolvedSubscription, SubscriptionFailureState, SubscriptionSource, WebhookDispatcher,
    WebhookSender,
};
use awaken_webhook_managed::WebhookOutboxNotifier;
use awaken_webhook_managed::{
    ConfigPlaneLifecycleDelivery, ConfigPlaneSubscriptionSource, config_plane_lifecycle_delivery,
    reconcile_webhook_inventory, recover_webhook_mutations, webhook_config_router,
    webhook_config_router_loopback,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Extension, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct MemStore(InMemoryWebhookStore);

#[derive(Default)]
struct SessionOutbox {
    rows: Mutex<HashMap<String, ManagedLifecycleFact>>,
    fail_completion_once: AtomicBool,
}

impl SessionOutbox {
    fn fail_completion_once(&self) {
        self.fail_completion_once.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ManagedSessionRepository for SessionOutbox {
    async fn create(
        &self,
        _owner_scope: &str,
        _session: PersistedSession,
        _idempotency: awaken_session_contract::IdempotencyRecord,
        _lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_contract::SessionCreateResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        Err(
            awaken_session_contract::SessionRepositoryError::Unavailable(
                "SessionOutbox test double does not own aggregate creation".into(),
            ),
        )
    }

    async fn replay_create(
        &self,
        _owner_scope: &str,
        _session_id: &str,
        _idempotency: &awaken_session_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, awaken_session_contract::SessionRepositoryError> {
        Err(
            awaken_session_contract::SessionRepositoryError::Unavailable(
                "SessionOutbox test double does not own aggregate creation".into(),
            ),
        )
    }

    async fn commit_mutation(
        &self,
        _owner_scope: &str,
        _mutation: awaken_session_contract::SessionMutation,
    ) -> Result<
        awaken_session_contract::SessionMutationResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        Err(
            awaken_session_contract::SessionRepositoryError::Unavailable(
                "SessionOutbox test double does not own aggregate mutation".into(),
            ),
        )
    }

    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.rows
            .lock()
            .unwrap()
            .entry(fact.id.clone())
            .or_insert(fact);
        Ok(())
    }
    async fn pending_lifecycle(
        &self,
    ) -> Result<Vec<ManagedLifecycleFact>, awaken_session_contract::SessionRepositoryError> {
        Ok(self.rows.lock().unwrap().values().cloned().collect())
    }
    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        if self.fail_completion_once.swap(false, Ordering::SeqCst) {
            return Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "injected lifecycle completion outage".into(),
                ),
            );
        }
        self.rows.lock().unwrap().remove(fact_id);
        Ok(())
    }
    async fn get(
        &self,
        _session_id: &str,
    ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    }

    async fn reconcilable_sessions(
        &self,
    ) -> Result<
        awaken_session_contract::SessionRecoveryScan,
        awaken_session_contract::SessionRepositoryError,
    > {
        Ok(awaken_session_contract::SessionRecoveryScan::default())
    }

    async fn sessions_referencing_credential_source(
        &self,
        _workspace_id: &str,
        _source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, awaken_session_contract::SessionRepositoryError> {
        Err(
            awaken_session_contract::SessionRepositoryError::Unavailable(
                "SessionOutbox test double does not own credential dependency indexing".into(),
            ),
        )
    }

    async fn idempotency_receipt(
        &self,
        _session_id: &str,
        _key: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionIdempotencyReceipt>,
        awaken_session_contract::SessionRepositoryError,
    > {
        Ok(None)
    }

    async fn owner(
        &self,
        _session_id: &str,
    ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    }
}

impl MemStore {
    fn put(&self, def: WebhookEndpointDef) {
        let intent = WebhookMutationIntent::create(def);
        self.0.begin_mutation(intent.clone()).unwrap();
        self.0.apply_mutation(&intent).unwrap();
        self.0.complete_mutation(&intent).unwrap();
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.0.get(id).unwrap()
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        self.0.list(workspace_id).unwrap()
    }
}

impl WebhookStore for MemStore {
    fn get(
        &self,
        id: &str,
    ) -> Result<Option<WebhookEndpointDef>, awaken_config_resolver::ConfigRepositoryError> {
        Ok(MemStore::get(self, id))
    }
    fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<WebhookEndpointDef>, awaken_config_resolver::ConfigRepositoryError> {
        Ok(MemStore::list(self, workspace_id))
    }
    fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, awaken_config_resolver::ConfigRepositoryError> {
        self.0.update_authored(patch)
    }
    fn begin_mutation(
        &self,
        intent: WebhookMutationIntent,
    ) -> Result<(), awaken_config_resolver::ConfigRepositoryError> {
        self.0.begin_mutation(intent)
    }
    fn apply_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), awaken_config_resolver::ConfigRepositoryError> {
        self.0.apply_mutation(intent)
    }
    fn pending_mutations(
        &self,
    ) -> Result<Vec<WebhookMutationIntent>, awaken_config_resolver::ConfigRepositoryError> {
        self.0.pending_mutations()
    }
    fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), awaken_config_resolver::ConfigRepositoryError> {
        self.0.complete_mutation(intent)
    }
    fn material_refs(
        &self,
    ) -> Result<Vec<SecretRef>, awaken_config_resolver::ConfigRepositoryError> {
        self.0.material_refs()
    }
    fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, awaken_config_resolver::ConfigRepositoryError> {
        self.0.record_delivery(id, outcome, failure_threshold)
    }
}

/// An in-memory secret vault. `failing` makes every `put` return a storage fault,
/// to exercise the seal-failure arm.
struct MemSecrets {
    map: Mutex<HashMap<String, String>>,
    failing: bool,
    fail_delete: AtomicBool,
    after_first_inventory: Mutex<Option<(SecretRef, String)>>,
}

impl MemSecrets {
    fn ok() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            failing: false,
            fail_delete: AtomicBool::new(false),
            after_first_inventory: Mutex::new(None),
        }
    }
    fn failing() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            failing: true,
            fail_delete: AtomicBool::new(false),
            after_first_inventory: Mutex::new(None),
        }
    }

    fn with_material_after_first_inventory(self, reference: SecretRef) -> Self {
        *self.after_first_inventory.lock().unwrap() =
            Some((reference, "concurrently-published".into()));
        self
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
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(CredentialError::Storage("delete failed".into()));
        }
        self.map.lock().unwrap().remove(&r.0);
        Ok(())
    }
    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        let inventory = self
            .map
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .map(SecretRef)
            .collect();
        if let Some((reference, material)) = self.after_first_inventory.lock().unwrap().take() {
            self.map.lock().unwrap().insert(reference.0, material);
        }
        Ok(inventory)
    }
}

fn seed(store: &MemStore, id: &str, ws: &str) {
    store.put(WebhookEndpointDef {
        id: id.into(),
        workspace_id: ws.into(),
        url: "https://old.example/hook".into(),
        event_types: vec!["run.completed".into()],
        disabled: false,
        consecutive_failures: 0,
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
    assert!(
        row.secret_ref.0.starts_with("sec:webhook:whref_"),
        "the secret-free row carries a fresh opaque material reference"
    );
    assert!(!row.secret_ref.0.contains(secret));
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
async fn malformed_authoring_fields_are_rejected_without_side_effects() {
    // Cause/effect graph: C1 event_types is not an array of non-empty strings;
    // C2 disabled is not boolean. Effects E1 400, E2 no row, E3 no material,
    // E4 no mutation intent. Decision table R1=C1 -> E1..E4; R2=C2 -> E1..E4.
    for body in [
        json!({"url":"https://x.example/hook","event_types":["ok", 7]}),
        json!({"url":"https://x.example/hook","event_types":[""]}),
        json!({"url":"https://x.example/hook","disabled":"false"}),
    ] {
        let store = Arc::new(MemStore::default());
        let secrets = Arc::new(MemSecrets::ok());
        let (status, _) = call(
            store.clone(),
            secrets.clone(),
            "PUT",
            "/v1/config/webhook-subscriptions/wh_invalid",
            Some("ws_a"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "R1/R2");
        assert!(store.get("wh_invalid").is_none(), "E2");
        assert!(secrets.map.lock().unwrap().is_empty(), "E3");
        assert!(store.pending_mutations().unwrap().is_empty(), "E4");
    }
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
    assert!(
        store.pending_mutations().unwrap().is_empty(),
        "successful compensation retires the create intent"
    );
}

#[tokio::test]
async fn failed_seal_and_failed_compensation_remain_recoverable() {
    // Cause/effect graph: C1 durable create intent; C2 secret put fails; C3
    // compensating delete also fails. Effects E1 500/no row, E2 intent retained;
    // after C3 clears, E3 recovery idempotently deletes and completes. Decision
    // rules R1=C1∧C2∧C3 -> E1∧E2; R2=R1∧¬C3 -> E3.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::failing());
    secrets.fail_delete.store(true, Ordering::SeqCst);
    let (status, _) = call(
        store.clone(),
        secrets.clone(),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://x.example/y" })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "R1/E1");
    assert!(store.get("wh1").is_none(), "R1/E1");
    assert_eq!(store.pending_mutations().unwrap().len(), 1, "R1/E2");

    secrets.fail_delete.store(false, Ordering::SeqCst);
    assert_eq!(
        recover_webhook_mutations(store.as_ref(), secrets.as_ref())
            .await
            .unwrap(),
        1,
        "R2/E3"
    );
    assert!(store.pending_mutations().unwrap().is_empty(), "R2/E3");
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
async fn owned_delete_removes_row_and_signing_material() {
    // Cause/effect graph: C1 owned row + sealed material; C2 DELETE; effects E1
    // journal before mutation, E2 row removal, E3 material removal, E4 journal
    // completion. Decision rule R1=C1∧C2 -> 204∧E2∧E3∧E4.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh1", "ws_a", &[]).await;
    let reference = store.get("wh1").unwrap().secret_ref;
    let (status, _) = call(
        store.clone(),
        secrets.clone(),
        "DELETE",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "R1");
    assert!(store.get("wh1").is_none(), "R1/E2");
    assert!(secrets.get(&reference).await.is_err(), "R1/E3");
    assert!(store.pending_mutations().unwrap().is_empty(), "R1/E4");
}

#[tokio::test]
async fn failed_delete_cleanup_keeps_a_recoverable_intent() {
    // Cause/effect decision table: R1 committed row + delete publication + vault
    // delete failure -> 500, row absent, material and intent retained; R2 vault
    // recovers -> recovery removes material and completes the intent.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh1", "ws_a", &[]).await;
    let reference = store.get("wh1").unwrap().secret_ref;
    secrets.fail_delete.store(true, Ordering::SeqCst);
    let (status, _) = call(
        store.clone(),
        secrets.clone(),
        "DELETE",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "R1");
    assert!(store.get("wh1").is_none(), "R1");
    assert!(secrets.get(&reference).await.is_ok(), "R1");
    assert_eq!(store.pending_mutations().unwrap().len(), 1, "R1");

    secrets.fail_delete.store(false, Ordering::SeqCst);
    assert_eq!(
        recover_webhook_mutations(store.as_ref(), secrets.as_ref())
            .await
            .unwrap(),
        1,
        "R2"
    );
    assert!(secrets.get(&reference).await.is_err(), "R2");
    assert!(store.pending_mutations().unwrap().is_empty(), "R2");
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
        consecutive_failures: 7,
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
    assert_eq!(row.consecutive_failures, 7, "failure state is preserved");
}

#[tokio::test]
async fn an_operator_can_reenable_an_auto_disabled_subscription() {
    // Cause/effect graph: C1 existing disabled row; C2 PUT omits `disabled`; C3
    // PUT explicitly sets false. Effects E1 preserve disabled/count (covered by
    // `update_in_place...`); E2 re-enable and reset count while preserving secret.
    // Decision rule R2=C1∧C3 -> E2 closes the auto-disable recovery loop.
    let store = Arc::new(MemStore::default());
    store.put(WebhookEndpointDef {
        id: "wh1".into(),
        workspace_id: "ws_a".into(),
        url: "https://old.example/hook".into(),
        event_types: vec![],
        disabled: true,
        consecutive_failures: 20,
        secret_ref: SecretRef("whsec:original".into()),
    });
    let (status, body) = call(
        store.clone(),
        Arc::new(MemSecrets::ok()),
        "PUT",
        "/v1/config/webhook-subscriptions/wh1",
        Some("ws_a"),
        Some(json!({ "url": "https://fixed.example/hook", "disabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R2");
    assert_eq!(body["disabled"], false, "R2");
    let row = store.get("wh1").unwrap();
    assert!(!row.disabled, "R2");
    assert_eq!(row.consecutive_failures, 0, "R2");
    assert_eq!(row.secret_ref.0, "whsec:original", "R2");
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
        consecutive_failures: 0,
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
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, "wh1");
    assert!(
        !out[0].secret.is_empty(),
        "the secret is materialized at delivery"
    );
}

#[tokio::test]
async fn matching_fails_closed_when_a_subscription_secret_is_unresolvable() {
    // Cause/effect graph: C1 matching durable row; C2 secret material absent;
    // E1 source error; E2 caller keeps the lifecycle outbox fact pending. Decision
    // rule R1=C1∧C2 -> E1 (never an empty match set that would imply delivery).
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    // Row present, but its secret was never sealed.
    seed(&store, "wh1", "ws_a");
    let error = source(store, secrets)
        .matching("ws_a", "run.completed")
        .await
        .unwrap_err();
    assert!(
        error.contains("wh1") && error.contains("not found"),
        "R1 reports the affected subscription without leaking material: {error}"
    );

    let store = Arc::new(MemStore::default());
    seed(&store, "wh1", "ws_a");
    let delivery = config_plane_lifecycle_delivery(
        store as Arc<dyn WebhookStore>,
        Arc::new(MemSecrets::ok()) as Arc<dyn SecretStore>,
        None,
    );
    let error = delivery
        .deliver(&ManagedLifecycleFact {
            id: "event_missing_secret".into(),
            object_id: "sesn_1".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "run.completed".into(),
            timestamp: 1,
            runtime_interval: None,
        })
        .await
        .unwrap_err();
    assert!(error.contains("wh1"), "R1/E2: {error}");
}

#[tokio::test]
async fn matching_skips_disabled_and_type_mismatched_subscriptions() {
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh_ok", "ws_a", &["run.completed"]).await;
    seed_resolvable(&store, &secrets, "wh_other", "ws_a", &["run.failed"]).await; // type mismatch
    seed_resolvable(&store, &secrets, "wh_dis", "ws_a", &["run.completed"]).await;
    store
        .update_authored(WebhookAuthoringPatch {
            id: "wh_dis".into(),
            workspace_id: "ws_a".into(),
            url: "https://x.example/hook".into(),
            event_types: vec!["run.completed".into()],
            disabled: Some(true),
        })
        .unwrap();

    let out = source(store, secrets)
        .matching("ws_a", "run.completed")
        .await
        .unwrap();
    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["wh_ok"],
        "only the enabled, type-matching endpoint"
    );
}

#[tokio::test]
async fn delivery_results_persist_failure_disable_and_success_reset() {
    // Cause/effect decision table: R1 failure below threshold -> durable count 1,
    // active; R2 next failure at threshold -> disabled; R3 success on an active
    // endpoint -> count reset. This proves the adapter delegates to the one store
    // transition rather than maintaining a process-local counter.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());
    seed_resolvable(&store, &secrets, "wh1", "ws_a", &[]).await;
    let source = source(store.clone(), secrets);
    assert_eq!(
        source.record_failure("wh1", 2).await.unwrap(),
        SubscriptionFailureState::Active,
        "R1"
    );
    assert_eq!(store.get("wh1").unwrap().consecutive_failures, 1, "R1");
    assert_eq!(
        source.record_failure("wh1", 2).await.unwrap(),
        SubscriptionFailureState::Disabled,
        "R2"
    );
    assert!(
        store.get("wh1").unwrap().disabled,
        "R2: auto-disable is a config-plane write"
    );

    seed_resolvable(&store, &MemSecrets::ok(), "wh2", "ws_a", &[]).await;
    source.record_failure("wh2", 2).await.unwrap();
    source.record_success("wh2").await.unwrap();
    assert_eq!(store.get("wh2").unwrap().consecutive_failures, 0, "R3");
}

#[tokio::test]
async fn interrupted_material_mutations_recover_from_the_durable_row() {
    // Cause/effect decision table:
    // R1 create intent + material + no row -> delete unpublished material;
    // R2 create intent + material + committed row -> retain material;
    // R3 delete intent + removed row + material -> delete retired material.
    // Every rule completes the intent; no process-local phase flag participates.
    let store = Arc::new(MemStore::default());
    let secrets = Arc::new(MemSecrets::ok());

    let create_def = |id: &str| WebhookEndpointDef {
        id: id.into(),
        workspace_id: "ws_a".into(),
        url: "https://hooks.example/hook".into(),
        event_types: vec![],
        disabled: false,
        consecutive_failures: 0,
        secret_ref: SecretRef(format!("sec:webhook:{id}")),
    };

    let unpublished = WebhookMutationIntent::create(create_def("unpublished"));
    store.begin_mutation(unpublished.clone()).unwrap();
    secrets
        .put(
            &unpublished.after.as_ref().unwrap().secret_ref,
            RedactedString::new("candidate"),
        )
        .await
        .unwrap();
    assert_eq!(
        recover_webhook_mutations(store.as_ref(), secrets.as_ref())
            .await
            .unwrap(),
        1,
        "R1"
    );
    assert!(
        secrets
            .get(&unpublished.after.as_ref().unwrap().secret_ref)
            .await
            .is_err(),
        "R1"
    );

    let published = WebhookMutationIntent::create(create_def("published"));
    let published_ref = published.after.as_ref().unwrap().secret_ref.clone();
    store.begin_mutation(published.clone()).unwrap();
    secrets
        .put(&published_ref, RedactedString::new("keep"))
        .await
        .unwrap();
    store.apply_mutation(&published).unwrap();
    assert_eq!(
        recover_webhook_mutations(store.as_ref(), secrets.as_ref())
            .await
            .unwrap(),
        1,
        "R2"
    );
    assert_eq!(
        secrets.get(&published_ref).await.unwrap().expose_secret(),
        "keep",
        "R2"
    );

    let before = store.get("published").unwrap();
    let deleted = WebhookMutationIntent::delete(before);
    store.begin_mutation(deleted.clone()).unwrap();
    store.apply_mutation(&deleted).unwrap();
    assert_eq!(
        recover_webhook_mutations(store.as_ref(), secrets.as_ref())
            .await
            .unwrap(),
        1,
        "R3"
    );
    assert!(secrets.get(&published_ref).await.is_err(), "R3");
    assert!(store.pending_mutations().unwrap().is_empty());
}

#[tokio::test]
async fn inventory_reconciliation_protects_intents_and_reports_missing_material() {
    // Cause graph: C1 unreferenced webhook key; C2 pending-create key; C3 committed
    // missing key; C4 another domain's key; C5 a committed key becomes visible
    // after the deletion snapshot. Effects E1 delete C1, E2 retain C2, E3 report
    // C3, E4 never touch C4, E5 do not falsely report C5. Decision table R1..R5
    // maps each cause to its effect and proves shared-vault ownership filtering
    // plus the two-snapshot concurrency boundary.
    let store = Arc::new(MemStore::default());
    let concurrent = SecretRef("sec:webhook:concurrent".into());
    let secrets =
        Arc::new(MemSecrets::ok().with_material_after_first_inventory(concurrent.clone()));
    let orphan = SecretRef("sec:webhook:orphan".into());
    let protected = SecretRef("sec:webhook:pending".into());
    let foreign = SecretRef("sec:cred:not-a-webhook".into());
    for reference in [&orphan, &protected, &foreign] {
        secrets
            .put(reference, RedactedString::new("material"))
            .await
            .unwrap();
    }
    let pending = WebhookMutationIntent::create(WebhookEndpointDef {
        id: "pending".into(),
        workspace_id: "ws_a".into(),
        url: "https://hooks.example/pending".into(),
        event_types: vec![],
        disabled: false,
        consecutive_failures: 0,
        secret_ref: protected.clone(),
    });
    store.begin_mutation(pending).unwrap();
    let missing = SecretRef("sec:webhook:missing".into());
    store.put(WebhookEndpointDef {
        id: "missing".into(),
        workspace_id: "ws_a".into(),
        url: "https://hooks.example/missing".into(),
        event_types: vec![],
        disabled: false,
        consecutive_failures: 0,
        secret_ref: missing.clone(),
    });
    store.put(WebhookEndpointDef {
        id: "concurrent".into(),
        workspace_id: "ws_a".into(),
        url: "https://hooks.example/concurrent".into(),
        event_types: vec![],
        disabled: false,
        consecutive_failures: 0,
        secret_ref: concurrent.clone(),
    });

    let report = reconcile_webhook_inventory(store.as_ref(), secrets.as_ref())
        .await
        .unwrap();
    assert_eq!(report.orphaned_deleted, vec![orphan.clone()], "R1/E1");
    assert!(secrets.get(&orphan).await.is_err(), "R1/E1");
    assert!(secrets.get(&protected).await.is_ok(), "R2/E2");
    assert_eq!(report.missing_material, vec![missing], "R3/E3");
    assert!(secrets.get(&foreign).await.is_ok(), "R4/E4");
    assert!(secrets.get(&concurrent).await.is_ok(), "R5/E5");
}

// --- ConfigPlaneLifecycleDelivery: the no-owner early return ---

struct CountingSource(Arc<Mutex<u32>>);
#[async_trait]
impl SubscriptionSource for CountingSource {
    async fn matching(&self, _ws: &str, _event: &str) -> Result<Vec<ResolvedSubscription>, String> {
        *self.0.lock().unwrap() += 1;
        Ok(Vec::new())
    }
    async fn record_success(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn record_failure(
        &self,
        _id: &str,
        _failure_threshold: u32,
    ) -> Result<SubscriptionFailureState, String> {
        Ok(SubscriptionFailureState::Active)
    }
}

struct NoopSender;
#[async_trait]
impl WebhookSender for NoopSender {
    async fn post(&self, _u: &str, _h: Vec<(String, String)>, _b: String) -> Result<u16, String> {
        Ok(200)
    }
}

#[tokio::test]
async fn delivery_without_a_workspace_owner_does_not_fan_out() {
    // Cause/effect rule R1: C1 a committed lifecycle fact has no Workspace
    // owner -> E1 Control delivery succeeds as a deterministic no-op and E2 no
    // subscription lookup occurs. Notification/replay is intentionally absent:
    // ownership filtering belongs to the delivery adapter, not the notifier.
    let calls = Arc::new(Mutex::new(0u32));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(CountingSource(calls.clone())),
        Arc::new(NoopSender),
    ));
    ConfigPlaneLifecycleDelivery::new(dispatcher, None)
        .deliver(&ManagedLifecycleFact {
            id: "session:sesn_1:created".into(),
            object_id: "sesn_1".into(),
            workspace_id: None,
            event_type: "session.created".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("an unattributed fact is a successful no-op");
    assert_eq!(*calls.lock().unwrap(), 0, "R1/E1");
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
    async fn matching(&self, _ws: &str, _event: &str) -> Result<Vec<ResolvedSubscription>, String> {
        Ok(vec![ResolvedSubscription {
            id: "wh1".into(),
            url: "https://x.example/hook".into(),
            secret: self.0.clone(),
        }])
    }
    async fn record_success(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn record_failure(
        &self,
        _id: &str,
        _failure_threshold: u32,
    ) -> Result<SubscriptionFailureState, String> {
        Ok(SubscriptionFailureState::Active)
    }
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

async fn shutdown_lifecycle(lifecycle: &awaken_service_lifecycle::ServiceLifecycle) {
    lifecycle
        .shutdown(std::time::Duration::from_secs(1))
        .await
        .expect("supervised webhook reconciliation exits after cancellation");
}

fn outbox_notifier(
    dispatcher: Arc<WebhookDispatcher>,
    org_id: Option<String>,
    outbox: Arc<dyn ManagedSessionRepository>,
    lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> WebhookOutboxNotifier {
    WebhookOutboxNotifier::with_delivery(
        Arc::new(ConfigPlaneLifecycleDelivery::new(dispatcher, org_id)),
        outbox,
        lifecycle,
    )
}

#[tokio::test]
async fn a_stable_fact_id_is_enqueued_and_delivered_only_once_per_pending_row() {
    // Decision rule R1: one transactionally committed lifecycle fact (C1),
    // followed by duplicate drain notifications (C2), yields one delivery (E1)
    // and retirement of the canonical session-outbox row (E2).
    // R1 also constrains C3 service cancellation -> E3 the sole drain exits;
    // there is no detached notification task to outlive the test/service.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_1:created".into(),
            object_id: "sesn_1".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.created".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));
    let notifier = outbox_notifier(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );

    notifier.notify();
    notifier.notify();
    wait_for_calls(&calls, 1).await;
    tokio::task::yield_now().await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .is_empty(),
        "successful delivery retires the row"
    );
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn failed_delivery_keeps_the_stable_fact_pending_for_recovery() {
    // Decision rule R2: a committed fact (C1) plus exhausted delivery retries
    // (C2) keeps that exact fact pending (E1) for later reconciliation.
    // C3 service cancellation stops retry ownership cleanly (E2); the durable
    // row, rather than a detached task, remains the recovery source.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_1:archived".into(),
            object_id: "sesn_1".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.archived".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Err("injected outage".into()),
        }),
    ));
    let notifier = outbox_notifier(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );

    notifier.notify();
    wait_for_calls(&calls, 3).await;
    tokio::task::yield_now().await;

    let pending = outbox.pending_lifecycle().await.expect("pending lifecycle");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "session:sesn_1:archived");
    assert_eq!(pending[0].object_id, "sesn_1");
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn completion_failure_replays_the_same_stable_fact_identity() {
    // Cause/effect decision rule R3: C1 delivery succeeds but C2 the repository
    // cannot durably complete its receipt -> E1 the exact fact stays pending;
    // C3 a later wake after repository recovery -> E2 the same id is delivered
    // again and then retired. C4 cancellation -> E3 the sole replay task joins.
    // At-least-once receivers therefore deduplicate by this aggregate-owned id;
    // the notifier never invents a replacement identity.
    #[derive(Default)]
    struct RecordingDelivery(Mutex<Vec<String>>);

    #[async_trait]
    impl LifecycleFactDelivery for RecordingDelivery {
        async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
            self.0.lock().unwrap().push(fact.id.clone());
            Ok(())
        }
    }

    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_receipt:archived".into(),
            object_id: "sesn_receipt".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.archived".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    outbox.fail_completion_once();
    let delivery = Arc::new(RecordingDelivery::default());
    let notifier = WebhookOutboxNotifier::with_delivery(
        delivery.clone(),
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while delivery.0.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup replay attempts delivery");
    assert_eq!(
        outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .len(),
        1,
        "R3/E1"
    );

    notifier.notify();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while delivery.0.lock().unwrap().len() < 2
            || !outbox
                .pending_lifecycle()
                .await
                .expect("pending lifecycle")
                .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replayed fact completes after repository recovery");
    assert_eq!(
        *delivery.0.lock().unwrap(),
        vec![
            "session:sesn_receipt:archived".to_string(),
            "session:sesn_receipt:archived".to_string(),
        ],
        "R3/E2"
    );
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn rebuilding_the_sink_drains_rows_left_by_the_prior_process() {
    // Decision rule R3: a lifecycle fact left pending before process start
    // (C1) is recovered by sink construction (E1) and retired after success (E2).
    // C2 lifecycle cancellation after recovery -> E3 the startup drain joins;
    // construction itself owns no hidden runtime handle.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_1:deleted".into(),
            object_id: "sesn_1".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.deleted".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));

    let _rebuilt = outbox_notifier(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );
    wait_for_calls(&calls, 1).await;
    tokio::task::yield_now().await;

    assert!(
        outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .is_empty(),
        "startup recovery retires a successfully redelivered row"
    );
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn session_local_outbox_is_drained_after_commit_before_notify_crash() {
    // Cause/effect rule R4: C1 a durable row exists and C2 its transient notify
    // was lost before service start -> E1 the supervised startup replay delivers
    // and retires it; C3 cancellation -> E2 the replay owner joins.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_tx:created".into(),
            object_id: "sesn_tx".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    // No sink existed at commit time: this is the exact former crash window.
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(CountingStatusSender {
            calls: calls.clone(),
            status: Ok(200),
        }),
    ));
    let _restarted = outbox_notifier(
        dispatcher,
        None,
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );

    wait_for_calls(&calls, 1).await;
    assert!(
        outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .is_empty()
    );
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn periodic_reconciliation_redelivers_without_restart_or_a_new_event() {
    // Cause/effect decision table:
    // R5 C1 initial delivery failure + C2 no later notification + C3 dependency
    // recovers -> E1 interval replay delivers and retires the row.
    // R6 C4 service cancellation at any point -> E2 the one reconciliation loop
    // exits and joins; no constructor-owned or per-event task survives.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let outbox = Arc::new(SessionOutbox::default());
    outbox
        .append_lifecycle(ManagedLifecycleFact {
            id: "session:sesn_retry:created".into(),
            object_id: "sesn_retry".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        })
        .await
        .expect("append lifecycle");
    let calls = Arc::new(AtomicUsize::new(0));
    let failing = Arc::new(AtomicBool::new(true));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(RecoveringSender {
            calls: calls.clone(),
            failing: failing.clone(),
        }),
    ));
    let _notifier = WebhookOutboxNotifier::with_delivery_interval(
        Arc::new(ConfigPlaneLifecycleDelivery::new(dispatcher, None)),
        outbox.clone() as Arc<dyn ManagedSessionRepository>,
        std::time::Duration::from_millis(10),
        &service_lifecycle,
    );
    wait_for_calls(&calls, 3).await;
    assert_eq!(
        outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .len(),
        1
    );

    failing.store(false, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !outbox
            .pending_lifecycle()
            .await
            .expect("pending lifecycle")
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("periodic reconciliation should redeliver after recovery");
    shutdown_lifecycle(&service_lifecycle).await;
}

#[tokio::test]
async fn committed_fact_identity_and_scope_are_projected_unchanged() {
    // Decision rule R7: C1 two transactionally committed facts carry stable,
    // aggregate-owned ids and C2 both have a Workspace owner; after one notifier
    // wake, E1 both are projected with those exact ids, object ids and scopes,
    // E2 Control stamps only the configured organization, and C3 cancellation
    // joins the sole replay task. The notifier must not mint another event id.
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let dispatcher = Arc::new(WebhookDispatcher::new(
        Arc::new(OneSubSource(awaken_webhook::generate_secret())),
        Arc::new(RecordingSender(tx)),
    ));
    let outbox = Arc::new(SessionOutbox::default());
    for fact in [
        ManagedLifecycleFact {
            id: "session:sesn_1:status_idled".into(),
            object_id: "sesn_1".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1_768_780_800,
            runtime_interval: None,
        },
        ManagedLifecycleFact {
            id: "session:sesn_2:status_terminated".into(),
            object_id: "sesn_2".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "session.status_terminated".into(),
            timestamp: 1_768_780_801,
            runtime_interval: None,
        },
    ] {
        outbox
            .append_lifecycle(fact)
            .await
            .expect("commit lifecycle fact");
    }
    let notifier = outbox_notifier(
        dispatcher,
        Some("org_root".into()),
        outbox as Arc<dyn ManagedSessionRepository>,
        &service_lifecycle,
    );

    notifier.notify();
    let mut delivered = Vec::new();
    for _ in 0..2 {
        let body = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("delivery within the deadline")
            .expect("a body was delivered");
        delivered.push(serde_json::from_str::<Value>(&body).unwrap());
    }
    delivered.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    assert_eq!(delivered[0]["type"], "event");
    assert_eq!(delivered[0]["id"], "session:sesn_1:status_idled");
    assert_eq!(delivered[0]["data"]["type"], "session.status_idled");
    assert_eq!(delivered[0]["data"]["id"], "sesn_1");
    assert_eq!(delivered[0]["data"]["workspace_id"], "ws_a");
    assert_eq!(delivered[0]["data"]["organization_id"], "org_root");
    assert_eq!(delivered[1]["id"], "session:sesn_2:status_terminated");
    assert_eq!(delivered[1]["data"]["id"], "sesn_2");
    shutdown_lifecycle(&service_lifecycle).await;
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

/// Drive one request through an arbitrary already-built router so the production
/// and loopback-only admission policies can be compared over the same command.
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

#[tokio::test]
async fn loopback_router_seam_changes_only_endpoint_admission() {
    // Causes: the fixtures below establish `loopback router seam` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: subscription scope, durable registration, and the signed delivery
    // contract remain separate authorities and must not be inferred from one another.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the production router receives a loopback URL; C2
    // the test-support router receives the same URL; C3 both commands carry the
    // same tenant and authoring fields. Effects: E1 production rejects without a
    // row; E2 test support admits one tenant-owned row.
    //
    // | Rule | C1 strict | C2 loopback | C3 same command | Effects |
    // |---|---|---|---|---|
    // | P1 | yes | no | yes | E1 |
    // | P2 | no | yes | yes | E2 |
    let strict_store = Arc::new(MemStore::default());
    let strict_router = webhook_config_router(
        strict_store.clone() as Arc<dyn WebhookStore>,
        Arc::new(MemSecrets::ok()) as Arc<dyn SecretStore>,
    );
    let loopback_store = Arc::new(MemStore::default());
    let loopback_router = webhook_config_router_loopback(
        loopback_store.clone() as Arc<dyn WebhookStore>,
        Arc::new(MemSecrets::ok()) as Arc<dyn SecretStore>,
    );
    let command = json!({
        "url": "https://127.0.0.1:9999/hook",
        "event_types": ["run.completed"]
    });

    assert_eq!(
        drive(
            strict_router,
            "PUT",
            "/v1/config/webhook-subscriptions/wh_policy",
            "ws_a",
            command.clone(),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "P1/E1 strict production admission rejects loopback"
    );
    assert!(strict_store.get("wh_policy").is_none(), "P1/E1");

    assert_eq!(
        drive(
            loopback_router,
            "PUT",
            "/v1/config/webhook-subscriptions/wh_policy",
            "ws_a",
            command,
        )
        .await,
        StatusCode::CREATED,
        "P2/E2 loopback-only admission accepts the fixture receiver"
    );
    assert_eq!(
        loopback_store
            .get("wh_policy")
            .expect("P2/E2 row is stored")
            .workspace_id,
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
