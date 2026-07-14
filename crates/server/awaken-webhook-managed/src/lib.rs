//! The managed webhook bridge: connects the managed session lifecycle
//! (protocol-managed's `SessionLifecycleSink` port) to `awaken-webhook`'s neutral
//! delivery machinery, backed by the **config plane**. Subscriptions are an
//! id-addressed config resource (`awaken-config-resolver`'s [`WebhookStore`],
//! durably the admin store) and their `whsec_` signing secret is sealed in the
//! [`SecretStore`] — this crate holds no store of its own. It exposes the
//! subscription front door (`/v1/config/webhook-subscriptions/…`, which mints +
//! seals the secret) and the guard→`WorkspaceScope` mapping.
//!
//! All open crates (config-resolver / credential-vault / agent-contract are the
//! read-side + vault ports, never the durable admin backend — that is injected by
//! the assembly), so the `awaken` / `awaken-server` management plane uses it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{WebhookEndpointDef, WebhookStore};
use awaken_credential_vault::{SecretRef, SecretStore};
use awaken_protocol_managed::{SessionLifecycleSink, WorkspaceScope};
use awaken_webhook::{
    ReqwestSender, ResolvedSubscription, SubscriptionSource, WebhookDispatcher, WebhookEvent,
    WebhookSender, generate_secret,
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
    dispatcher: Arc<WebhookDispatcher>,
    /// The owning org, stamped on every event — `None` self-hosted (ADR-0048 D4/D6).
    org_id: Option<String>,
    /// Monotonic event-id source (`event_<n>`).
    seq: AtomicU64,
}

impl WebhookLifecycleSink {
    pub fn new(dispatcher: Arc<WebhookDispatcher>, org_id: Option<String>) -> Self {
        Self {
            dispatcher,
            org_id,
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl SessionLifecycleSink for WebhookLifecycleSink {
    async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str) {
        // No owner → no workspace to fan out to (the bare pre-owner surface).
        let Some(workspace_id) = workspace_id else {
            return;
        };
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let event = WebhookEvent::new(
            format!("event_{n}"),
            rfc3339(now),
            event_type,
            session_id,
            workspace_id,
            self.org_id.clone(),
        );
        // Deliver out-of-band: the session create must not wait on HTTP egress.
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            dispatcher.dispatch(&event, now).await;
        });
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
/// [`ResolvedSubscription`]. Auto-disable is a config-plane write (flip `disabled`
/// and put the row back). The secret is materialized only here, at delivery — the
/// row and the CRUD responses stay secret-free.
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
    async fn matching(&self, workspace_id: &str, event_type: &str) -> Vec<ResolvedSubscription> {
        let mut out = Vec::new();
        for def in self.store.list(workspace_id) {
            if def.disabled || !def.wants(event_type) {
                continue;
            }
            // An unresolvable secret can never sign — skip the endpoint this dispatch.
            let Ok(secret) = self.secrets.get(&def.secret_ref).await else {
                continue;
            };
            out.push(ResolvedSubscription {
                id: def.id,
                url: def.url,
                secret: secret.expose_secret().to_string(),
            });
        }
        out
    }

    async fn disable(&self, id: &str) {
        if let Some(mut def) = self.store.get(id) {
            def.disabled = true;
            self.store.put(def);
        }
    }
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
    let event_types: Vec<String> = body
        .get("event_types")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let workspace_id = scope_of(scope);

    // Update in place: keep the sealed secret + owner. Self-fence on the row's owner
    // so a caller cannot hijack another tenant's id (belt-and-suspenders with the
    // management-plane resource-ownership guard). 404, never 403 — no existence
    // disclosure.
    if let Some(existing) = state.store.get(&id) {
        if existing.workspace_id != workspace_id {
            return (StatusCode::NOT_FOUND, Json(not_found()));
        }
        let updated = WebhookEndpointDef {
            id: id.clone(),
            workspace_id: existing.workspace_id,
            url,
            event_types,
            disabled: existing.disabled,
            secret_ref: existing.secret_ref,
        };
        state.store.put(updated.clone());
        return (StatusCode::OK, Json(view(&updated)));
    }

    // Create: mint the `whsec_` secret, seal it in the vault, store the secret-free
    // row referencing it. The plaintext is returned exactly once, here.
    let secret = generate_secret();
    let secret_ref = SecretRef(format!("whsec:{id}"));
    if state
        .secrets
        .put(&secret_ref, RedactedString::new(secret.clone()))
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({ "error": { "type": "api_error", "message": "could not seal the signing secret" } }),
            ),
        );
    }
    let def = WebhookEndpointDef {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        url: url.clone(),
        event_types: event_types.clone(),
        disabled: false,
        secret_ref,
    };
    state.store.put(def);
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
        Some(def) if def.workspace_id == workspace_id => Ok(Json(view(&def))),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

async fn list_subscriptions(
    State(state): State<WebhookCrudState>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Json<Value> {
    let data: Vec<Value> = state
        .store
        .list(&scope_of(scope))
        .iter()
        .map(view)
        .collect();
    Json(json!({ "data": data, "has_more": false }))
}

async fn delete_subscription(
    State(state): State<WebhookCrudState>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> StatusCode {
    // Only unsubscribe an endpoint this tenant owns; a cross-tenant or absent id is a
    // silent no-op (idempotent, and no ownership disclosure).
    if let Some(def) = state.store.get(&id)
        && def.workspace_id == scope_of(scope)
    {
        state.store.delete(&id);
    }
    StatusCode::NO_CONTENT
}

/// Build the lifecycle sink + subscription front door over the config-plane
/// `store` (durably the admin store) and the `secrets` vault (shared with the rest
/// of the config plane). The dispatcher reads matching subscriptions and resolves
/// their secrets through the same `store`/`secrets`, so the CRUD writes and the
/// dispatcher reads see one row set.
pub fn assemble(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    // The production posture: the sender enforces the delivery-time SSRF /
    // DNS-rebinding guard (resolve-and-pin to globally-routable addresses), and the
    // CRUD front door rejects a non-https / private / loopback endpoint at admission.
    assemble_with(
        store,
        secrets,
        org_id,
        Arc::new(ReqwestSender::guarded()),
        strict_endpoint_url_policy(),
    )
}

/// [`assemble`] for delivery to a **loopback** receiver: the permissive
/// [`ReqwestSender::default`] plus an admission policy that admits any URL. The
/// guarded production posture pins to globally-routable addresses and rejects a
/// `127.0.0.1` endpoint at both admission and delivery, so an in-process e2e (its
/// receiver is a real loopback axum server) wires this instead. Never the production
/// path — [`assemble`] is.
pub fn assemble_loopback(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    assemble_with(
        store,
        secrets,
        org_id,
        Arc::new(ReqwestSender::default()),
        Arc::new(|_url| Ok(())),
    )
}

fn assemble_with(
    store: Arc<dyn WebhookStore>,
    secrets: Arc<dyn SecretStore>,
    org_id: Option<String>,
    sender: Arc<dyn WebhookSender>,
    url_policy: EndpointUrlPolicy,
) -> (Arc<WebhookLifecycleSink>, Router) {
    let source = Arc::new(ConfigPlaneSubscriptionSource::new(
        store.clone(),
        secrets.clone(),
    ));
    let dispatcher = Arc::new(WebhookDispatcher::new(source, sender));
    let sink = Arc::new(WebhookLifecycleSink::new(dispatcher, org_id));
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
