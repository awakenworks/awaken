//! The managed webhook bridge: connects the managed session lifecycle
//! (protocol-managed's `SessionLifecycleSink` port) to `awaken-webhook`'s neutral
//! delivery machinery, and exposes the workspace-scoped subscription CRUD +
//! the guard→`WorkspaceScope` mapping. This is the ONLY place that knows both
//! the managed port and the delivery crate — the wire crate stays
//! webhook-agnostic, the webhook crate stays protocol-neutral. All open crates,
//! so both `awaken-server-local` and the BuSL-free `awaken-standalone` use it.
//!
//! Env-gated: [`webhook_plane`] returns `Some` only when `AWAKEN_WEBHOOK_DIR` is
//! set (durable subscriptions under `webhooks.db`); unset = no webhook plane.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_protocol_managed::SessionLifecycleSink;
use awaken_webhook::{
    InMemoryWebhookRepository, ReqwestSender, SqliteWebhookRepository, WebhookDispatcher,
    WebhookEvent, WebhookRepository, WebhookSubscription, generate_secret,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Assembly-layer glue (aspect → core seam): map the guard's edge-resolved
/// [`awaken_authz_enforce::RequestTenancy`] to the wire crate's
/// [`awaken_protocol_managed::WorkspaceScope`], so `create_session` receives the
/// owning workspace without either crate depending on the other. Apply this as a
/// layer INSIDE the guard (guard resolves tenancy → this maps it → handler reads
/// it). A request with no resolved tenancy passes through unstamped.
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
            .insert(awaken_protocol_managed::WorkspaceScope(
                tenancy.workspace_id,
            ));
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

/// The workspace-scoped subscription CRUD surface (ADR-0048 D3 path shape):
/// `/v1/workspaces/{ws}/webhooks`. POST mints a `whsec_` secret (returned once),
/// GET lists, DELETE removes.
pub fn webhook_router(repo: Arc<dyn WebhookRepository>) -> Router {
    Router::new()
        .route(
            "/v1/workspaces/:ws/webhooks",
            post(create_subscription).get(list_subscriptions),
        )
        .route(
            "/v1/workspaces/:ws/webhooks/:id",
            axum::routing::delete(delete_subscription),
        )
        .with_state(WebhookCrudState {
            repo,
            seq: Arc::new(AtomicU64::new(0)),
        })
}

#[derive(Clone)]
struct WebhookCrudState {
    repo: Arc<dyn WebhookRepository>,
    seq: Arc<AtomicU64>,
}

async fn create_subscription(
    State(state): State<WebhookCrudState>,
    Path(ws): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // `url` is required; `event_types` optional (empty = all types).
    let Some(url) = body.get("url").and_then(Value::as_str).map(str::to_string) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": { "type": "invalid_request_error", "message": "`url` is required" } }),
            ),
        );
    };
    let event_types: Vec<String> = body
        .get("event_types")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let id = format!("wh_{}", state.seq.fetch_add(1, Ordering::SeqCst));
    let secret = generate_secret();
    state
        .repo
        .upsert(WebhookSubscription {
            id: id.clone(),
            workspace_id: ws.clone(),
            url: url.clone(),
            secret: secret.clone(),
            event_types: event_types.clone(),
            disabled: false,
        })
        .await;
    // The secret is returned exactly once, on create (never re-fetchable).
    (
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "workspace_id": ws,
            "url": url,
            "secret": secret,
            "event_types": event_types,
        })),
    )
}

async fn list_subscriptions(
    State(state): State<WebhookCrudState>,
    Path(ws): Path<String>,
) -> Json<Value> {
    let data: Vec<Value> = state
        .repo
        .list(&ws)
        .await
        .into_iter()
        // The secret is never echoed back after create.
        .map(|s| {
            json!({
                "id": s.id,
                "workspace_id": s.workspace_id,
                "url": s.url,
                "event_types": s.event_types,
                "disabled": s.disabled,
            })
        })
        .collect();
    Json(json!({ "data": data, "has_more": false }))
}

async fn delete_subscription(
    State(state): State<WebhookCrudState>,
    Path((_ws, id)): Path<(String, String)>,
) -> StatusCode {
    state.repo.delete(&id).await;
    StatusCode::NO_CONTENT
}

/// Assemble the webhook plane from the environment (ADR-0048 / S10). `Some` when
/// `AWAKEN_WEBHOOK_DIR` is set (durable `webhooks.db`); the returned sink is wired
/// into the managed state and the router merged into the surface. `None` = no
/// webhook plane (default). `AWAKEN_ORG_ID`, if set, stamps a cloud org on events.
pub fn webhook_plane() -> Option<(Arc<WebhookLifecycleSink>, Router)> {
    let dir = std::env::var("AWAKEN_WEBHOOK_DIR").ok()?;
    let repo: Arc<dyn WebhookRepository> = Arc::new(
        SqliteWebhookRepository::open(&format!("{dir}/webhooks.db"))
            .expect("open webhooks.db under AWAKEN_WEBHOOK_DIR"),
    );
    Some(assemble(repo, std::env::var("AWAKEN_ORG_ID").ok()))
}

/// Build the sink + CRUD router over `repo` (shared so the CRUD writes and the
/// dispatcher reads the same subscriptions). Factored out so tests assemble it
/// over an in-memory repo without touching the environment.
pub fn assemble(
    repo: Arc<dyn WebhookRepository>,
    org_id: Option<String>,
) -> (Arc<WebhookLifecycleSink>, Router) {
    let dispatcher = Arc::new(WebhookDispatcher::new(
        repo.clone(),
        Arc::new(ReqwestSender::default()),
    ));
    let sink = Arc::new(WebhookLifecycleSink::new(dispatcher, org_id));
    (sink, webhook_router(repo))
}

/// An in-memory webhook plane (tests): CRUD + sink over one shared repo.
pub fn assemble_in_memory(org_id: Option<String>) -> (Arc<WebhookLifecycleSink>, Router) {
    assemble(Arc::new(InMemoryWebhookRepository::default()), org_id)
}
