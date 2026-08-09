//! The managed webhook bridge: connects the managed session lifecycle (the
//! `SessionLifecycleSink` port, in `awaken-session-contract`) to `awaken-webhook`'s
//! neutral delivery machinery, backed by the **config plane**. Subscriptions are an
//! id-addressed config resource (`awaken-config-resolver`'s [`WebhookStore`],
//! durably the admin store) and their `whsec_` signing secret is sealed in the
//! [`SecretStore`] — this crate holds no store of its own. It exposes the
//! subscription front door (`/v1/config/webhook-subscriptions/…`, which mints +
//! seals the secret) and the guard→`WorkspaceScope` mapping.
//!
//! All open crates (config-resolver / credential-vault / agent-contract are the
//! read-side + vault ports, never the durable admin backend — that is injected by
//! the assembly), so the `awaken` / `awaken-coordinator` management plane uses it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{
    ConfigRepositoryError, WebhookAuthoringPatch, WebhookAuthoringState, WebhookDeliveryOutcome,
    WebhookDeliveryState, WebhookEndpointDef, WebhookMutationIntent, WebhookStore,
};
use awaken_credential_vault::{SecretRef, SecretStore};
use awaken_session_contract::{
    ManagedLifecycleFact, ManagedSessionRepository, SessionLifecycleSink,
};
use awaken_tenancy::WorkspaceScope;
use awaken_webhook::{
    ReqwestSender, ResolvedSubscription, SubscriptionFailureState, SubscriptionSource,
    WebhookDispatcher, WebhookEvent, WebhookSender, generate_secret, generate_secret_reference,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

/// Assembly-layer glue (aspect → core seam): map the guard's edge-resolved
/// [`awaken_authz_enforce::RequestTenancy`] to the wire crate's [`WorkspaceScope`],
/// so `create_session` — and the webhook front door — receive the owning workspace
/// without either crate depending on the other. Apply this as a layer INSIDE the
/// guard (guard resolves tenancy → this maps it → handler reads it). A request with
/// no resolved tenancy passes through unstamped.
pub async fn stamp_workspace_scope(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(tenancy) = request
        .extensions()
        .get::<awaken_authz_enforce::RequestTenancy>()
        .cloned()
    {
        request
            .extensions_mut()
            .insert(WorkspaceScope(tenancy.workspace_id));
    }
    next.run(request).await
}

/// Bridges the managed session lifecycle to the webhook dispatcher: on a committed
/// lifecycle fact it builds the Anthropic-shaped event (stamping the session's
/// owner) and delivers out-of-band, so a slow endpoint never blocks the session.
pub struct WebhookLifecycleSink {
    delivery: Arc<dyn LifecycleFactDelivery>,
    /// Monotonic event-id source (`event_<n>`).
    seq: AtomicU64,
    session_outbox: Arc<dyn ManagedSessionRepository>,
    draining: Arc<tokio::sync::Mutex<()>>,
}

/// Delivery half of the lifecycle projection. AllInOne injects the local
/// config-plane dispatcher; split Coordinator injects an authenticated Control
/// adapter. The Session outbox and its completion rules remain here exactly once.
#[async_trait::async_trait]
pub trait LifecycleFactDelivery: Send + Sync {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String>;
}

struct ConfigPlaneLifecycleDelivery {
    dispatcher: Arc<WebhookDispatcher>,
    org_id: Option<String>,
}

#[async_trait::async_trait]
impl LifecycleFactDelivery for ConfigPlaneLifecycleDelivery {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        let Some(workspace_id) = fact.workspace_id.clone() else {
            return Ok(());
        };
        let event = WebhookEvent::new(
            fact.id.clone(),
            rfc3339(fact.timestamp),
            fact.event_type.clone(),
            fact.object_id.clone(),
            workspace_id,
            self.org_id.clone(),
        );
        let report = self
            .dispatcher
            .dispatch(&event, fact.timestamp)
            .await
            .map_err(|error| error.to_string())?;
        if report.failed.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} webhook subscription(s) remain pending",
                report.failed.len()
            ))
        }
    }
}

/// Build the Control-owned delivery adapter without acquiring a Session
/// repository. A split Control exposes this adapter behind its private service
/// router; Coordinator remains the sole owner of the durable lifecycle outbox.
pub fn config_plane_lifecycle_delivery(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
) -> Arc<dyn LifecycleFactDelivery> {
    let source = Arc::new(ConfigPlaneSubscriptionSource::new(store, secrets));
    Arc::new(ConfigPlaneLifecycleDelivery {
        dispatcher: Arc::new(WebhookDispatcher::new(
            source,
            Arc::new(ReqwestSender::guarded()),
        )),
        org_id,
    })
}

const RECONCILIATION_INTERVAL: Duration = Duration::from_secs(30);

impl WebhookLifecycleSink {
    /// Lifecycle facts are read from the same repository transaction that commits
    /// the Session transition. This is the only webhook outbox authority.
    pub fn new(
        dispatcher: Arc<WebhookDispatcher>,
        org_id: Option<String>,
        outbox: Arc<dyn ManagedSessionRepository>,
    ) -> Self {
        Self::with_delivery(
            Arc::new(ConfigPlaneLifecycleDelivery { dispatcher, org_id }),
            outbox,
        )
    }

    #[must_use]
    pub fn with_delivery(
        delivery: Arc<dyn LifecycleFactDelivery>,
        outbox: Arc<dyn ManagedSessionRepository>,
    ) -> Self {
        Self::with_delivery_interval(delivery, outbox, RECONCILIATION_INTERVAL)
    }

    #[doc(hidden)]
    pub fn with_session_outbox_interval(
        dispatcher: Arc<WebhookDispatcher>,
        org_id: Option<String>,
        outbox: Arc<dyn ManagedSessionRepository>,
        interval: Duration,
    ) -> Self {
        Self::with_delivery_interval(
            Arc::new(ConfigPlaneLifecycleDelivery { dispatcher, org_id }),
            outbox,
            interval,
        )
    }

    fn with_delivery_interval(
        delivery: Arc<dyn LifecycleFactDelivery>,
        outbox: Arc<dyn ManagedSessionRepository>,
        interval: Duration,
    ) -> Self {
        let sink = Self {
            delivery,
            seq: AtomicU64::new(0),
            session_outbox: outbox,
            draining: Arc::new(tokio::sync::Mutex::new(())),
        };
        sink.spawn_drain();
        sink.spawn_reconciliation(interval);
        sink
    }

    fn spawn_reconciliation(&self, interval: Duration) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let delivery = self.delivery.clone();
        let session_outbox = self.session_outbox.clone();
        let draining = self.draining.clone();
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                Self::drain_once(&delivery, &session_outbox, &draining).await;
            }
        });
    }

    fn spawn_drain(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let delivery = self.delivery.clone();
        let session_outbox = self.session_outbox.clone();
        let draining = self.draining.clone();
        handle.spawn(async move {
            Self::drain_once(&delivery, &session_outbox, &draining).await;
        });
    }

    async fn drain_once(
        delivery: &Arc<dyn LifecycleFactDelivery>,
        session_outbox: &Arc<dyn ManagedSessionRepository>,
        draining: &Arc<tokio::sync::Mutex<()>>,
    ) {
        let _guard = draining.lock().await;
        for row in session_outbox.pending_lifecycle().await {
            if delivery.deliver(&row).await.is_ok() {
                session_outbox.complete_lifecycle(&row.id).await;
            }
        }
    }
}

#[async_trait::async_trait]
impl SessionLifecycleSink for WebhookLifecycleSink {
    async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.session_outbox
            .append_lifecycle(ManagedLifecycleFact {
                id: format!("event_{n}"),
                object_id: session_id.to_string(),
                workspace_id: workspace_id.map(str::to_string),
                event_type: event_type.to_string(),
                timestamp: now,
            })
            .await;
        self.spawn_drain();
    }

    async fn emit_fact(
        &self,
        fact_id: &str,
        session_id: &str,
        workspace_id: Option<&str>,
        event_type: &str,
    ) {
        // The Session mutation already committed this exact fact in its repository
        // transaction. The sink is a notification/drain adapter only; appending it
        // again would recreate the former parallel outbox path.
        let _ = (fact_id, session_id, workspace_id, event_type);
        self.spawn_drain();
    }
}

/// RFC-3339 UTC from unix seconds, allocation-free of any date crate (Howard
/// Hinnant's public-domain `civil_from_days`), matching `awaken-authz-enforce`.
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The dispatcher's read/lifecycle port over the config plane: enumerate a
/// workspace's [`WebhookEndpointDef`]s, drop disabled / type-mismatched ones, and
/// resolve each `secret_ref` through the [`SecretStore`] into a signable
/// [`ResolvedSubscription`]. Auto-disable is one atomic config-plane delivery-state
/// transition. The secret is materialized only here, at delivery — the row and the
/// CRUD responses stay secret-free.
pub struct ConfigPlaneSubscriptionSource {
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
}

impl ConfigPlaneSubscriptionSource {
    pub fn new(store: Arc<dyn WebhookStore>, secrets: Arc<dyn SecretStore>) -> Self {
        Self { store, secrets }
    }
}

#[async_trait::async_trait]
impl SubscriptionSource for ConfigPlaneSubscriptionSource {
    async fn matching(
        &self,
        workspace_id: &str,
        event_type: &str,
    ) -> Result<Vec<ResolvedSubscription>, String> {
        let mut out = Vec::new();
        for def in self
            .store
            .list(workspace_id)
            .map_err(|error| error.to_string())?
        {
            if def.disabled || !def.wants(event_type) {
                continue;
            }
            // A missing/unavailable secret authority is not an empty subscription:
            // falsely omitting the row would retire the durable event. Keep the
            // outbox fact pending until the authoritative material is recoverable
            // or an operator disables/deletes the subscription.
            let secret =
                self.secrets.get(&def.secret_ref).await.map_err(|error| {
                    format!("could not resolve subscription {}: {error}", def.id)
                })?;
            out.push(ResolvedSubscription {
                id: def.id,
                url: def.url,
                secret: secret.expose_secret().to_string(),
            });
        }
        Ok(out)
    }

    async fn record_success(&self, id: &str) -> Result<(), String> {
        self.store
            .record_delivery(id, WebhookDeliveryOutcome::Succeeded, 1)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn record_failure(
        &self,
        id: &str,
        failure_threshold: u32,
    ) -> Result<SubscriptionFailureState, String> {
        self.store
            .record_delivery(id, WebhookDeliveryOutcome::Failed, failure_threshold)
            .map(|state| match state {
                WebhookDeliveryState::Active { .. } => SubscriptionFailureState::Active,
                WebhookDeliveryState::Disabled { .. } => SubscriptionFailureState::Disabled,
                WebhookDeliveryState::Missing => SubscriptionFailureState::Removed,
            })
            .map_err(|error| error.to_string())
    }
}

/// Result of reconciling webhook-owned material in the shared SecretStore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookInventoryReport {
    pub orphaned_deleted: Vec<SecretRef>,
    pub missing_material: Vec<SecretRef>,
}

fn material_set(definition: Option<&WebhookEndpointDef>) -> HashSet<SecretRef> {
    definition
        .map(|definition| HashSet::from([definition.secret_ref.clone()]))
        .unwrap_or_default()
}

async fn cleanup_material_difference(
    remove_from: Option<&WebhookEndpointDef>,
    retain_from: Option<&WebhookEndpointDef>,
    secrets: &dyn SecretStore,
) -> Result<(), String> {
    let remove = material_set(remove_from);
    let retain = material_set(retain_from);
    for reference in remove.difference(&retain) {
        secrets
            .delete(reference)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Resolve every interrupted create/delete after restart. The exact committed
/// row decides which side's material is retained; an unrelated row revision is a
/// conflict and is never guessed through.
pub async fn recover_webhook_mutations(
    store: &dyn WebhookStore,
    secrets: &dyn SecretStore,
) -> Result<usize, String> {
    let intents = store
        .pending_mutations()
        .map_err(|error| error.to_string())?;
    let mut recovered = 0;
    for intent in intents {
        let id = intent.id().map_err(|error| error.to_string())?;
        let current = store.get(id).map_err(|error| error.to_string())?;
        if current == intent.after {
            cleanup_material_difference(intent.before.as_ref(), intent.after.as_ref(), secrets)
                .await?;
        } else if current == intent.before {
            cleanup_material_difference(intent.after.as_ref(), intent.before.as_ref(), secrets)
                .await?;
        } else {
            return Err(format!(
                "webhook {id} no longer matches its pending material mutation"
            ));
        }
        store
            .complete_mutation(&intent)
            .map_err(|error| error.to_string())?;
        recovered += 1;
    }
    Ok(recovered)
}

/// Delete unreferenced webhook-owned keys and report committed references whose
/// material is absent. The first inventory snapshot bounds deletion candidates;
/// a second snapshot after deletion is the authority for missing-material
/// reporting, so a concurrently published key is not diagnosed from stale data.
pub async fn reconcile_webhook_inventory(
    store: &dyn WebhookStore,
    secrets: &dyn SecretStore,
) -> Result<WebhookInventoryReport, String> {
    let inventory = secrets
        .inventory()
        .await
        .map_err(|error| error.to_string())?;
    let pending = store
        .pending_mutations()
        .map_err(|error| error.to_string())?;
    let committed = store.material_refs().map_err(|error| error.to_string())?;
    let committed_keys: HashSet<String> = committed.iter().map(|item| item.0.clone()).collect();
    let protected: HashSet<String> = committed_keys
        .iter()
        .cloned()
        .chain(
            pending
                .iter()
                .flat_map(WebhookMutationIntent::material_refs)
                .map(|reference| reference.0),
        )
        .collect();
    let mut orphaned_deleted = Vec::new();
    for reference in inventory {
        if (reference.0.starts_with("sec:webhook:") || reference.0.starts_with("whsec:"))
            && !protected.contains(&reference.0)
        {
            secrets
                .delete(&reference)
                .await
                .map_err(|error| error.to_string())?;
            orphaned_deleted.push(reference);
        }
    }
    let present_after_reconciliation: HashSet<String> = secrets
        .inventory()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|reference| reference.0)
        .collect();
    let missing_material = committed
        .into_iter()
        .filter(|reference| !present_after_reconciliation.contains(&reference.0))
        .collect();
    Ok(WebhookInventoryReport {
        orphaned_deleted,
        missing_material,
    })
}

/// The workspace-scoped subscription front door, a config resource beside
/// mcp-servers / inference-profiles (ADR-0048 + ADR-0043 path shape):
/// `/v1/config/webhook-subscriptions[/{id}]`. PUT creates/updates (minting +
/// sealing a `whsec_` secret on first create, returned once); GET/LIST are
/// secret-free; DELETE unsubscribes. The owning workspace is the edge-stamped
/// [`WorkspaceScope`]; the assembly layers the tenant-ownership fence over it.
pub fn webhook_config_router(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
) -> Router {
    webhook_config_router_with_policy(store, secrets, strict_endpoint_url_policy())
}

/// [`webhook_config_router`] with an injectable endpoint-URL admission `policy`, so a
/// loopback e2e can register its `127.0.0.1` receiver. Production goes through
/// [`webhook_config_router`] (the strict SSRF policy).
fn webhook_config_router_with_policy(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    validate_url: EndpointUrlPolicy,
) -> Router {
    Router::new()
        .route("/v1/config/webhook-subscriptions", get(list_subscriptions))
        .route(
            "/v1/config/webhook-subscriptions/{id}",
            put(put_subscription)
                .get(get_subscription)
                .delete(delete_subscription),
        )
        .with_state(WebhookCrudState {
            store,
            secrets,
            validate_url,
        })
}

/// The endpoint-URL admission policy: `Ok(())` to admit, `Err(message)` to reject at
/// `PUT` time. Production is the SSRF check ([`strict_endpoint_url_policy`]); a
/// loopback e2e swaps in a permissive one.
type EndpointUrlPolicy = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// The production SSRF admission policy: reject a non-https scheme or a
/// private/loopback/metadata host (never stored, never fetched).
fn strict_endpoint_url_policy() -> EndpointUrlPolicy {
    Arc::new(|url| awaken_webhook::validate_endpoint_url(url).map_err(|e| e.to_string()))
}

#[derive(Clone)]
struct WebhookCrudState {
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    validate_url: EndpointUrlPolicy,
}

/// The seeded scope an unscoped (single-tenant / flat) request resolves to, kept in
/// step with the managed session + resource-owner default so a bare deployment is
/// self-consistent.
const DEFAULT_SCOPE: &str = "default";

fn scope_of(ext: Option<Extension<WorkspaceScope>>) -> String {
    ext.map(|Extension(WorkspaceScope(ws))| ws)
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string())
}

/// The standard not-found envelope — used for a genuine miss and for a cross-tenant
/// access alike, so ownership is never disclosed (404, never 403).
fn not_found() -> Value {
    json!({ "error": { "type": "not_found_error", "message": "webhook subscription not found" } })
}

fn repository_problem(error: ConfigRepositoryError) -> (StatusCode, Json<Value>) {
    let (status, message) = match error {
        ConfigRepositoryError::MutationConflict(_) => (
            StatusCode::CONFLICT,
            "webhook subscription mutation is already in progress",
        ),
        ConfigRepositoryError::Storage(_) | ConfigRepositoryError::InvalidMutation(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "webhook repository unavailable",
        ),
    };
    (
        status,
        Json(json!({"error":{"type":"api_error","message":message}})),
    )
}

/// A secret-free projection of the row (never echoes the signing secret).
fn view(def: &WebhookEndpointDef) -> Value {
    json!({
        "id": def.id,
        "workspace_id": def.workspace_id,
        "url": def.url,
        "event_types": def.event_types,
        "disabled": def.disabled,
    })
}

async fn put_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(url) = body.get("url").and_then(Value::as_str).map(str::to_string) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": { "type": "invalid_request_error", "message": "`url` is required" } }),
            ),
        );
    };
    // Fail closed on an SSRF-shaped endpoint: the dispatcher fetches this URL
    // server-side, so a non-https scheme or a private/loopback/metadata host is
    // rejected at admission (never stored, never fetched). The policy is a seam —
    // production wires the strict SSRF check; a loopback e2e permits its receiver.
    if let Err(rejected) = (state.validate_url)(&url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "type": "invalid_request_error", "message": rejected }
            })),
        );
    }
    let event_types: Vec<String> = match body.get("event_types") {
        None => Vec::new(),
        Some(Value::Array(values))
            if values
                .iter()
                .all(|value| value.as_str().is_some_and(|event| !event.trim().is_empty())) =>
        {
            values
                .iter()
                .map(|value| value.as_str().expect("validated string").to_string())
                .collect()
        }
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": { "type": "invalid_request_error", "message": "`event_types` must be an array of non-empty strings" }
                })),
            );
        }
    };
    if body
        .get("disabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "type": "invalid_request_error", "message": "`disabled` must be a boolean" }
            })),
        );
    }

    let workspace_id = scope_of(scope);

    // The repository owns the atomic read/modify/write. Delivery counters and the
    // secret reference can therefore change concurrently without being overwritten
    // by this authored patch.
    let patch = WebhookAuthoringPatch {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        url: url.clone(),
        event_types: event_types.clone(),
        disabled: body.get("disabled").and_then(Value::as_bool),
    };
    match state.store.update_authored(patch.clone()) {
        Ok(WebhookAuthoringState::Updated(updated)) => {
            return (StatusCode::OK, Json(view(&updated)));
        }
        Ok(WebhookAuthoringState::OwnerMismatch) => {
            return (StatusCode::NOT_FOUND, Json(not_found()));
        }
        Ok(WebhookAuthoringState::Missing) => {}
        Err(error) => return repository_problem(error),
    }

    // Create is a recoverable saga. The durable intent precedes the SecretStore
    // effect; its fresh reference prevents compensation from touching another
    // concurrent attempt's material.
    let secret = generate_secret();
    let secret_ref = SecretRef(format!("sec:webhook:{}", generate_secret_reference()));
    let def = WebhookEndpointDef {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        url: url.clone(),
        event_types: event_types.clone(),
        disabled: false,
        consecutive_failures: 0,
        secret_ref: secret_ref.clone(),
    };
    let intent = WebhookMutationIntent::create(def.clone());
    if let Err(error) = state.store.begin_mutation(intent.clone()) {
        // A concurrent create may have committed between update_authored(Missing)
        // and begin_mutation. Re-apply the patch once through the same atomic path.
        if matches!(error, ConfigRepositoryError::MutationConflict(_)) {
            match state.store.update_authored(patch) {
                Ok(WebhookAuthoringState::Updated(updated)) => {
                    return (StatusCode::OK, Json(view(&updated)));
                }
                Ok(WebhookAuthoringState::OwnerMismatch) => {
                    return (StatusCode::NOT_FOUND, Json(not_found()));
                }
                Ok(WebhookAuthoringState::Missing) | Err(_) => {}
            }
        }
        return repository_problem(error);
    }
    if let Err(error) = state
        .secrets
        .put(&secret_ref, RedactedString::new(secret.clone()))
        .await
    {
        // A failed put may have written before losing its response. Retire the
        // intent only after idempotent cleanup succeeds; otherwise recovery owns it.
        if state.secrets.delete(&secret_ref).await.is_ok() {
            let _ = state.store.complete_mutation(&intent);
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": { "type": "api_error", "message": format!("could not seal the signing secret: {error}") }
            })),
        );
    }
    if let Err(error) = state.store.apply_mutation(&intent) {
        // Never compensate an ambiguous row commit inline: the durable intent lets
        // recovery decide whether the row or the candidate material won.
        return repository_problem(error);
    }
    if let Err(error) = state.store.complete_mutation(&intent) {
        // Both externally visible projections committed. Preserve the successful
        // create response; recovery idempotently retires the stale journal entry.
        eprintln!("webhook mutation completion remains pending for {id}: {error}");
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "workspace_id": workspace_id,
            "url": url,
            "event_types": event_types,
            "disabled": false,
            "secret": secret,
        })),
    )
}

async fn get_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Value>, StatusCode> {
    let workspace_id = scope_of(scope);
    match state.store.get(&id) {
        // Self-fence: another tenant's id is a 404, not a disclosure.
        Ok(Some(def)) if def.workspace_id == workspace_id => Ok(Json(view(&def))),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

async fn list_subscriptions(
    State(state): State<WebhookCrudState>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Value>, StatusCode> {
    let rows = state
        .store
        .list(&scope_of(scope))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let data: Vec<Value> = rows.iter().map(view).collect();
    Ok(Json(json!({ "data": data, "has_more": false })))
}

async fn delete_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> StatusCode {
    // Only unsubscribe an endpoint this tenant owns; a cross-tenant or absent id is a
    // silent no-op (idempotent, and no ownership disclosure).
    let workspace_id = scope_of(scope);
    for _ in 0..3 {
        let definition = match state.store.get(&id) {
            Ok(Some(definition)) if definition.workspace_id == workspace_id => definition,
            Ok(_) => return StatusCode::NO_CONTENT,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
        };
        let intent = WebhookMutationIntent::delete(definition.clone());
        match state.store.begin_mutation(intent.clone()) {
            Ok(()) => {
                if state.store.apply_mutation(&intent).is_err() {
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                if state.secrets.delete(&definition.secret_ref).await.is_err() {
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                if let Err(error) = state.store.complete_mutation(&intent) {
                    eprintln!("webhook delete completion remains pending for {id}: {error}");
                }
                return StatusCode::NO_CONTENT;
            }
            Err(ConfigRepositoryError::MutationConflict(_)) => continue,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    StatusCode::CONFLICT
}

/// Build the lifecycle sink + subscription front door over the config-plane
/// `store` (durably the admin store) and the `secrets` vault (shared with the rest
/// of the config plane). The dispatcher reads matching subscriptions and resolves
/// their secrets through the same `store`/`secrets`, so the CRUD writes and the
/// dispatcher reads see one row set.
/// Production assembly using the session repository's transactional outbox.
pub fn assemble_with_session_repo(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
    sessions: Arc<dyn ManagedSessionRepository>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    assemble_with(
        store,
        secrets,
        org_id,
        Arc::new(ReqwestSender::guarded()),
        strict_endpoint_url_policy(),
        sessions,
    )
}

/// [`assemble`] for delivery to a **loopback** receiver: the permissive
/// [`ReqwestSender::default`] plus an admission policy that admits any URL. The
/// guarded production posture pins to globally-routable addresses and rejects a
/// `127.0.0.1` endpoint at both admission and delivery, so an in-process e2e (its
/// receiver is a real loopback axum server) wires this instead. Never the production
/// path — [`assemble_with_session_repo`] is.
#[cfg(feature = "test-support")]
pub fn assemble_loopback(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
    sessions: Arc<dyn ManagedSessionRepository>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    assemble_with(
        store,
        secrets,
        org_id,
        Arc::new(ReqwestSender::default()),
        Arc::new(|_url| Ok(())),
        sessions,
    )
}

fn assemble_with(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
    sender: Arc<dyn WebhookSender>,
    url_policy: EndpointUrlPolicy,
    sessions: Arc<dyn ManagedSessionRepository>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    let source = Arc::new(ConfigPlaneSubscriptionSource::new(
        store.clone(),
        secrets.clone(),
    ));
    let dispatcher = Arc::new(WebhookDispatcher::new(source, sender));
    let sink = Arc::new(WebhookLifecycleSink::new(dispatcher, org_id, sessions));
    (
        sink,
        webhook_config_router_with_policy(store, secrets, url_policy),
    )
}

#[cfg(test)]
mod rfc3339_tests {
    use super::rfc3339;

    /// The hand-rolled `civil_from_days` date math stamps `created_at` on every
    /// delivered event, so pin it against authoritative unix→RFC-3339 UTC vectors
    /// covering the boundaries that break naive implementations: day rollovers,
    /// a leap day, a century-leap year (2000), a non-leap century (2100), and a
    /// negative (pre-epoch) instant (the `div_euclid`/`rem_euclid` path).
    #[test]
    fn matches_known_unix_to_rfc3339_vectors() {
        let cases = [
            (0_i64, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (1_709_208_000, "2024-02-29T12:00:00Z"), // leap day
            (1_583_020_800, "2020-03-01T00:00:00Z"), // day after the 2020 leap day
            (951_825_600, "2000-02-29T12:00:00Z"),   // 2000 is a leap year (÷400)
            (4_102_444_800, "2100-01-01T00:00:00Z"), // 2100 is NOT a leap year (÷100)
            (-1, "1969-12-31T23:59:59Z"),            // pre-epoch, negative seconds
        ];
        for (secs, expected) in cases {
            assert_eq!(rfc3339(secs), expected, "rfc3339({secs})");
        }
    }
}
