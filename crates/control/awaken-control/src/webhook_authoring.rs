// @formal entrypoint webhook_authoring.material_mutation_is_row_authoritative
// Control owns workspace-scoped authoring and secret-material recovery. Webhook
// transport and Coordinator outbox remain injected. CRUD and crash/retry tests
// cover this adapter; the linked model is not a direct production proof.
//! Control-owned webhook subscription authoring and material reconciliation.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{
    ConfigRepositoryError, WebhookAuthoringPatch, WebhookAuthoringState, WebhookDeliveryOutcome,
    WebhookDeliveryState, WebhookEndpointDef, WebhookMutationIntent, WebhookStore,
};
use awaken_credential_vault::{SecretRef, SecretStore};
use awaken_tenancy::WorkspaceScope;
use awaken_webhook::{
    ResolvedSubscription, SubscriptionFailureState, SubscriptionSource, generate_secret,
    generate_secret_reference,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{Value, json};

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
            .await
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
            .await
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
            .await
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
        .await
        .map_err(|error| error.to_string())?;
    let mut recovered = 0;
    for intent in intents {
        let id = intent.id().map_err(|error| error.to_string())?;
        let current = store.get(id).await.map_err(|error| error.to_string())?;
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
            .await
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
        .await
        .map_err(|error| error.to_string())?;
    let committed = store
        .material_refs()
        .await
        .map_err(|error| error.to_string())?;
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

/// Loopback-only authoring router for integration tests. Production always uses
/// [`webhook_config_router`]'s strict endpoint policy.
#[cfg(feature = "test-support")]
pub fn webhook_config_router_loopback(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
) -> Router {
    webhook_config_router_with_policy(store, secrets, Arc::new(|_url| Ok(())))
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

/// The Workspace the trusted ingress edge already selected for this request.
///
/// There is no absent case to default: a subscription is an owned, tenant-fenced
/// resource, so an unstamped request is refused with `404` before any handler
/// body runs. A single-machine deployment gets its Workspace from the one
/// composition-root stamp
/// (`awaken_coordinator::workspace_path::with_platform_workspace`), never from a
/// compiled-in constant here.
pub struct RequiredWorkspaceScope(pub String);

impl<S> axum::extract::FromRequestParts<S> for RequiredWorkspaceScope
where
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        match parts
            .extensions
            .get::<WorkspaceScope>()
            .and_then(WorkspaceScope::non_empty)
        {
            Some(workspace) => Ok(Self(workspace.to_owned())),
            None => Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": {
                    "type": "not_found_error",
                    "message": "workspace not found"
                }})),
            )
                .into_response()),
        }
    }
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
        "consecutive_failures": def.consecutive_failures,
    })
}

async fn put_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
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
    // Read the authored delivery switch exactly once, here, and let both the
    // update patch and the create definition below consume that one value. The
    // create path used to hardcode `false`, so `PUT` of a NEW subscription
    // asking to be created disabled silently registered an enabled endpoint and
    // started delivering to it — the validation just above proves the field is
    // an input, which is precisely what made ignoring it invisible.
    let authored_disabled = body.get("disabled").and_then(Value::as_bool);

    // The repository owns the atomic read/modify/write. Delivery counters and the
    // secret reference can therefore change concurrently without being overwritten
    // by this authored patch.
    let patch = WebhookAuthoringPatch {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        url: url.clone(),
        event_types: event_types.clone(),
        disabled: authored_disabled,
    };
    match state.store.update_authored(patch.clone()).await {
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
        disabled: authored_disabled.unwrap_or(false),
        consecutive_failures: 0,
        secret_ref: secret_ref.clone(),
    };
    let intent = WebhookMutationIntent::create(def.clone());
    if let Err(error) = state.store.begin_mutation(intent.clone()).await {
        // A concurrent create may have committed between update_authored(Missing)
        // and begin_mutation. Re-apply the patch once through the same atomic path.
        if matches!(error, ConfigRepositoryError::MutationConflict(_)) {
            match state.store.update_authored(patch).await {
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
            let _ = state.store.complete_mutation(&intent).await;
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": { "type": "api_error", "message": format!("could not seal the signing secret: {error}") }
            })),
        );
    }
    if let Err(error) = state.store.apply_mutation(&intent).await {
        // Never compensate an ambiguous row commit inline: the durable intent lets
        // recovery decide whether the row or the candidate material won.
        return repository_problem(error);
    }
    if let Err(error) = state.store.complete_mutation(&intent).await {
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
            "disabled": def.disabled,
            "consecutive_failures": 0,
            "secret": secret,
        })),
    )
}

async fn get_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
) -> Result<Json<Value>, StatusCode> {
    match state.store.get(&id).await {
        // Self-fence: another tenant's id is a 404, not a disclosure.
        Ok(Some(def)) if def.workspace_id == workspace_id => Ok(Json(view(&def))),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

async fn list_subscriptions(
    State(state): State<WebhookCrudState>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
) -> Result<Json<Value>, StatusCode> {
    let rows = state
        .store
        .list(&workspace_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let data: Vec<Value> = rows.iter().map(view).collect();
    Ok(Json(json!({ "data": data, "has_more": false })))
}

async fn delete_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
) -> StatusCode {
    // Only unsubscribe an endpoint this tenant owns; a cross-tenant or absent id is a
    // silent no-op (idempotent, and no ownership disclosure).
    for _ in 0..3 {
        let definition = match state.store.get(&id).await {
            Ok(Some(definition)) if definition.workspace_id == workspace_id => definition,
            Ok(_) => return StatusCode::NO_CONTENT,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
        };
        let intent = WebhookMutationIntent::delete(definition.clone());
        match state.store.begin_mutation(intent.clone()).await {
            Ok(()) => {
                if state.store.apply_mutation(&intent).await.is_err() {
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                if state.secrets.delete(&definition.secret_ref).await.is_err() {
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                if let Err(error) = state.store.complete_mutation(&intent).await {
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
