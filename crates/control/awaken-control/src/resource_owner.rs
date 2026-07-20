//! Tenant ownership for the id-addressed management config resources (ADR-0051):
//! MCP server defs (`/v1/config/mcp-servers/{id}`), inference profiles
//! (`/v1/config/inference-profiles/{id}`), and webhook subscriptions
//! (`/v1/config/webhook-subscriptions/{id}`, ADR-0048).
//!
//! These live behind the cross-crate `awaken-config-resolver` stores (keyed by id
//! only), and the config-layer admin crate cannot see the agents-layer edge scope.
//! So ownership is enforced here, at the authoring plane, by a small middleware that
//! wraps the admin router: it records the authoring scope of each id on a
//! successful `PUT`, and answers a cross-tenant `GET`/`PUT` with **404** (never
//! 403 — no existence disclosure). The scope is the edge-stamped [`WorkspaceScope`]
//! (from workspace path addressing or an API key), defaulting to the seeded scope
//! for a single-tenant deployment, so such a deployment never fences itself.
//!
//! The model catalog (providers/endpoints/offerings) is intentionally NOT covered:
//! it is org/deployment-level shared configuration, not a per-workspace resource.
//!
//! The sibling memory-store ownership guard (server-side, over the process-global
//! store) lives in the data-plane crate; this crate owns only the config-resource
//! fence the authoring router applies.

use std::sync::{Arc, Mutex};

use awaken_protocol_managed::WorkspaceScope;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The seeded scope an unscoped (single-tenant / flat) request resolves to, kept
/// in step with the managed session default so a bare deployment is self-consistent.
const DEFAULT_SCOPE: &str = "default";

/// The id→owner-scope index for the covered config resources, shared across
/// requests. Cloneable (an `Arc`), so it is both middleware state and, in tests,
/// inspectable.
#[derive(Clone)]
pub struct ResourceOwners(Arc<OwnerRepository>);

enum OwnerRepository {
    Memory(Mutex<std::collections::HashMap<String, String>>),
    Sqlite(Mutex<rusqlite::Connection>),
}

impl Default for ResourceOwners {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceOwners {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(OwnerRepository::Memory(Mutex::new(
            std::collections::HashMap::new(),
        ))))
    }

    /// Open the durable owner projection shared by every management process.
    /// Resource payload stores remain independent; this table is the PEP's
    /// persisted subject-to-resource assignment and contains no permissions.
    #[must_use]
    pub fn open_at(dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(dir).expect("create management owner directory");
        let connection = rusqlite::Connection::open(dir.join("resource-owners.sqlite"))
            .expect("open management owner database");
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS resource_owners (\
                     resource_key TEXT PRIMARY KEY,\
                     workspace_id TEXT NOT NULL\
                 );",
            )
            .expect("migrate management resource owners");
        Self(Arc::new(OwnerRepository::Sqlite(Mutex::new(connection))))
    }

    #[must_use]
    pub fn open_from_env() -> Self {
        std::env::var("AWAKEN_MGMT_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map_or_else(Self::new, |dir| Self::open_at(std::path::Path::new(&dir)))
    }

    fn owner(&self, key: &str) -> Option<String> {
        match self.0.as_ref() {
            OwnerRepository::Memory(rows) => rows
                .lock()
                .expect("resource owners poisoned")
                .get(key)
                .cloned(),
            OwnerRepository::Sqlite(connection) => connection
                .lock()
                .expect("resource owners poisoned")
                .query_row(
                    "SELECT workspace_id FROM resource_owners WHERE resource_key = ?1",
                    rusqlite::params![key],
                    |row| row.get(0),
                )
                .ok(),
        }
    }

    fn record(&self, key: String, scope: String) {
        match self.0.as_ref() {
            OwnerRepository::Memory(rows) => {
                rows.lock()
                    .expect("resource owners poisoned")
                    .insert(key, scope);
            }
            OwnerRepository::Sqlite(connection) => {
                connection
                    .lock()
                    .expect("resource owners poisoned")
                    .execute(
                        "INSERT INTO resource_owners(resource_key, workspace_id) VALUES (?1, ?2) \
                         ON CONFLICT(resource_key) DO UPDATE SET workspace_id = excluded.workspace_id",
                        rusqlite::params![key, scope],
                    )
                    .expect("persist management resource owner");
            }
        }
    }
}

/// The middleware: fence a cross-tenant request to an owned config resource, and
/// record ownership on a successful author (`PUT`).
pub async fn resource_ownership_guard(
    State(owners): State<ResourceOwners>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let scope = request
        .extensions()
        .get::<WorkspaceScope>()
        .map(|w| w.0.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
    if request.method() == Method::GET && path == "/v1/config/mcp-servers" {
        let response = next.run(request).await;
        return filter_mcp_list(response, &owners, &scope).await;
    }
    let Some(key) = owned_resource_key(&path) else {
        return next.run(request).await;
    };
    let method = request.method().clone();
    match owners.owner(&key) {
        Some(owner) if owner != scope => return not_found(),
        None if method != Method::PUT => return not_found(),
        _ => {}
    }
    let response = next.run(request).await;
    // Record ownership on a first successful author, so a later cross-tenant
    // access is fenced. (A same-scope re-author just re-records the same owner.)
    if method == Method::PUT && response.status().is_success() {
        owners.record(key, scope);
    }
    response
}

/// The ownership key (`"mcp:{id}"` / `"profile:{id}"`) for a covered config
/// resource path, or `None` for any other path. Resolve sub-actions inherit their
/// parent key; collection and org-shared catalog routes remain unkeyed.
fn owned_resource_key(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "v1" || segments.next()? != "config" {
        return None;
    }
    let family = segments.next()?;
    if family == "agents" {
        let id = segments.next().filter(|value| !value.is_empty())?;
        return matches!(
            (segments.next(), segments.next()),
            (Some("mcp"), None | Some("resolve"))
        )
        .then(|| format!("agent-mcp:{id}"));
    }
    let kind = match family {
        "mcp-servers" => "mcp",
        "inference-profiles" => "profile",
        "webhook-subscriptions" => "webhook",
        _ => return None,
    };
    let id = segments.next().filter(|s| !s.is_empty())?;
    // Sub-actions inherit the parent resource's owner. Extra nested paths are not
    // part of these resource APIs and still pass to the router's 404.
    if segments
        .next()
        .is_some_and(|action| !matches!(action, "resolve" | "resolve-candidates"))
    {
        return None;
    }
    Some(format!("{kind}:{id}"))
}

async fn filter_mcp_list(response: Response, owners: &ResourceOwners, scope: &str) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 8 << 20).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(mut rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    rows.retain(|row| {
        row.get("id")
            .and_then(|id| {
                id.as_str()
                    .or_else(|| id.get(0).and_then(|value| value.as_str()))
            })
            .is_some_and(|id| owners.owner(&format!("mcp:{id}")).as_deref() == Some(scope))
    });
    Json(rows).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": "resource not found" }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_only_the_owned_id_resources() {
        assert_eq!(
            owned_resource_key("/v1/config/mcp-servers/calc"),
            Some("mcp:calc".to_string())
        );
        assert_eq!(
            owned_resource_key("/v1/config/inference-profiles/p1"),
            Some("profile:p1".to_string())
        );
        // Collection/shared routes are unkeyed; sub-actions inherit the parent key.
        assert_eq!(owned_resource_key("/v1/config/mcp-servers"), None);
        assert_eq!(
            owned_resource_key("/v1/config/inference-profiles/p1/resolve"),
            Some("profile:p1".to_string())
        );
        assert_eq!(owned_resource_key("/v1/config/catalog"), None);
        assert_eq!(owned_resource_key("/v1/config/providers/anthropic"), None);
        assert_eq!(owned_resource_key("/v1/agents/x"), None);
    }

    #[test]
    fn keys_webhook_subscriptions_and_ignores_empty_or_trailing_ids() {
        // Webhook subscriptions (ADR-0048) are the third owned resource family;
        // dropping this key would silently un-fence webhooks across tenants — a
        // tenant-isolation fail-open. Lock the mapping and the empty-id guard.
        assert_eq!(
            owned_resource_key("/v1/config/webhook-subscriptions/wh1"),
            Some("webhook:wh1".to_string())
        );
        // A trailing empty id (no id segment) is not an owned resource → None,
        // so it passes through rather than fencing on the empty key `"webhook:"`.
        assert_eq!(
            owned_resource_key("/v1/config/webhook-subscriptions/"),
            None
        );
        assert_eq!(owned_resource_key("/v1/config/webhook-subscriptions"), None);
        // A sub-action under a webhook id is not the bare resource → None.
        assert_eq!(
            owned_resource_key("/v1/config/webhook-subscriptions/wh1/rotate"),
            None
        );
    }

    #[test]
    fn owner_records_and_fences_by_scope() {
        let owners = ResourceOwners::new();
        owners.record("mcp:calc".into(), "tenant-a".into());
        assert_eq!(owners.owner("mcp:calc").as_deref(), Some("tenant-a"));
        assert!(owners.owner("mcp:calc") != Some("tenant-b".to_string()));
        assert_eq!(owners.owner("mcp:unknown"), None);
    }

    // ---- CEG §10 additions: the middleware end-to-end (F33/F34) ------------

    use axum::Router;
    use axum::body::Body;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    /// The ownership guard over a terminal handler that echoes the status named
    /// by the `x-status` header (default 200) — so a test can drive a failed
    /// author (OW5) as well as a successful one.
    fn app(owners: ResourceOwners) -> Router {
        async fn echo(request: Request) -> Response {
            let status = request
                .headers()
                .get("x-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| StatusCode::from_bytes(s.as_bytes()).ok())
                .unwrap_or(StatusCode::OK);
            status.into_response()
        }
        Router::new()
            .fallback(echo)
            .layer(axum::middleware::from_fn_with_state(
                owners,
                resource_ownership_guard,
            ))
    }

    /// Drive one request, optionally stamping a `WorkspaceScope` and forcing the
    /// downstream handler's status; returns the response status.
    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        scope: Option<&str>,
        status: Option<u16>,
    ) -> StatusCode {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if let Some(code) = status {
            b = b.header("x-status", code.to_string());
        }
        let mut req = b.body(Body::empty()).unwrap();
        if let Some(scope) = scope {
            req.extensions_mut()
                .insert(WorkspaceScope(scope.to_string()));
        }
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        // Drain the body so the connection future completes cleanly.
        let _ = resp.into_body().collect().await;
        status
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow1_collection_and_shared_paths_pass_through() {
        let owners = ResourceOwners::new();
        owners.record("mcp:calc".into(), "tenant-a".into());
        let app = app(owners);
        for (m, uri) in [
            ("GET", "/v1/config/mcp-servers"),
            ("GET", "/v1/config/catalog"),
        ] {
            assert_eq!(
                call(&app, m, uri, Some("tenant-b"), None).await,
                StatusCode::OK,
                "{m} {uri}"
            );
        }
        assert_eq!(
            call(
                &app,
                "POST",
                "/v1/config/inference-profiles/p1/resolve",
                Some("tenant-b"),
                None,
            )
            .await,
            StatusCode::NOT_FOUND,
            "an unowned legacy sub-action fails closed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow2_first_successful_put_records_the_owner() {
        let owners = ResourceOwners::new();
        let app = app(owners.clone());
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/mcp-servers/calc",
                Some("tenant-a"),
                None
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(owners.owner("mcp:calc").as_deref(), Some("tenant-a"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow3_cross_tenant_access_is_404_no_disclosure() {
        let owners = ResourceOwners::new();
        owners.record("mcp:calc".into(), "tenant-a".into());
        let app = app(owners);
        // Neither a cross-tenant read nor a cross-tenant author is admitted —
        // and both answer 404, not 403 (no existence disclosure).
        assert_eq!(
            call(
                &app,
                "GET",
                "/v1/config/mcp-servers/calc",
                Some("tenant-b"),
                None
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/mcp-servers/calc",
                Some("tenant-b"),
                None
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow7_webhook_subscription_is_tenant_fenced() {
        // The same cross-tenant fence must cover webhook subscriptions: a first
        // author records the owner, then a foreign tenant's read AND author both
        // answer 404 (no existence disclosure) — matching profiles/MCP (ow3).
        let owners = ResourceOwners::new();
        let app = app(owners.clone());
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/webhook-subscriptions/wh1",
                Some("tenant-a"),
                None
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(owners.owner("webhook:wh1").as_deref(), Some("tenant-a"));
        assert_eq!(
            call(
                &app,
                "GET",
                "/v1/config/webhook-subscriptions/wh1",
                Some("tenant-b"),
                None
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/webhook-subscriptions/wh1",
                Some("tenant-b"),
                None
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow4_owner_reaccess_passes_through() {
        let owners = ResourceOwners::new();
        owners.record("mcp:calc".into(), "tenant-a".into());
        let app = app(owners);
        assert_eq!(
            call(
                &app,
                "GET",
                "/v1/config/mcp-servers/calc",
                Some("tenant-a"),
                None
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/mcp-servers/calc",
                Some("tenant-a"),
                None
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow5_get_or_failed_put_does_not_record_ownership() {
        let owners = ResourceOwners::new();
        let app = app(owners.clone());
        // An unowned legacy read fails closed and never authors ownership…
        assert_eq!(
            call(
                &app,
                "GET",
                "/v1/config/mcp-servers/calc",
                Some("tenant-a"),
                None
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(owners.owner("mcp:calc"), None);
        // …nor does a PUT that the handler rejected.
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/config/mcp-servers/calc",
                Some("tenant-a"),
                Some(400)
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(owners.owner("mcp:calc"), None);
    }

    #[test]
    fn owner_projection_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let owners = ResourceOwners::open_at(dir.path());
        owners.record("mcp:calc".into(), "tenant-a".into());
        drop(owners);

        let reopened = ResourceOwners::open_at(dir.path());
        assert_eq!(reopened.owner("mcp:calc").as_deref(), Some("tenant-a"));
        assert_eq!(reopened.owner("mcp:unknown"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ow6_default_single_tenant_never_self_fences() {
        // No `WorkspaceScope` stamp → the seeded DEFAULT_SCOPE, throughout: a
        // bare single-tenant deployment authors, re-reads, and re-authors freely.
        let owners = ResourceOwners::new();
        let app = app(owners.clone());
        assert_eq!(
            call(&app, "PUT", "/v1/config/mcp-servers/calc", None, None).await,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "GET", "/v1/config/mcp-servers/calc", None, None).await,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "PUT", "/v1/config/mcp-servers/calc", None, None).await,
            StatusCode::OK
        );
        assert_eq!(owners.owner("mcp:calc").as_deref(), Some(DEFAULT_SCOPE));
    }
}
