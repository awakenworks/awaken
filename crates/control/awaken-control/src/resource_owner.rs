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

use std::collections::HashMap;
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
#[derive(Clone, Default)]
pub struct ResourceOwners(Arc<Mutex<HashMap<String, String>>>);

impl ResourceOwners {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn owner(&self, key: &str) -> Option<String> {
        self.0
            .lock()
            .expect("resource owners poisoned")
            .get(key)
            .cloned()
    }

    fn record(&self, key: String, scope: String) {
        self.0
            .lock()
            .expect("resource owners poisoned")
            .insert(key, scope);
    }
}

/// The middleware: fence a cross-tenant request to an owned config resource, and
/// record ownership on a successful author (`PUT`).
pub async fn resource_ownership_guard(
    State(owners): State<ResourceOwners>,
    request: Request,
    next: Next,
) -> Response {
    let Some(key) = owned_resource_key(request.uri().path()) else {
        return next.run(request).await;
    };
    let scope = request
        .extensions()
        .get::<WorkspaceScope>()
        .map(|w| w.0.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
    // Fence: a known owner other than this scope → 404, before the handler runs.
    if let Some(owner) = owners.owner(&key)
        && owner != scope
    {
        return not_found();
    }
    let method = request.method().clone();
    let response = next.run(request).await;
    // Record ownership on a first successful author, so a later cross-tenant
    // access is fenced. (A same-scope re-author just re-records the same owner.)
    if method == Method::PUT && response.status().is_success() {
        owners.record(key, scope);
    }
    response
}

/// The ownership key (`"mcp:{id}"` / `"profile:{id}"`) for a covered config
/// resource path, or `None` for any other path (list routes, the resolve
/// sub-actions, the shared catalog, and everything else pass through unfenced).
fn owned_resource_key(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "v1" || segments.next()? != "config" {
        return None;
    }
    let kind = match segments.next()? {
        "mcp-servers" => "mcp",
        "inference-profiles" => "profile",
        "webhook-subscriptions" => "webhook",
        _ => return None,
    };
    let id = segments.next().filter(|s| !s.is_empty())?;
    // Only the bare `/{id}` resource is owned; sub-actions (e.g. `/resolve`) pass
    // through — they are reads gated by the same middleware on the parent id in
    // practice, and never author ownership.
    if segments.next().is_some() {
        return None;
    }
    Some(format!("{kind}:{id}"))
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
        // List routes, sub-actions, the shared catalog, and unrelated paths: None.
        assert_eq!(owned_resource_key("/v1/config/mcp-servers"), None);
        assert_eq!(
            owned_resource_key("/v1/config/inference-profiles/p1/resolve"),
            None
        );
        assert_eq!(owned_resource_key("/v1/config/catalog"), None);
        assert_eq!(owned_resource_key("/v1/config/providers/anthropic"), None);
        assert_eq!(owned_resource_key("/v1/agents/x"), None);
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
    async fn ow1_list_and_subaction_paths_pass_through_unfenced() {
        // Even with an owner on file for one scope, the un-keyed paths (list, a
        // resolve sub-action, the shared catalog) never fence another scope.
        let owners = ResourceOwners::new();
        owners.record("mcp:calc".into(), "tenant-a".into());
        let app = app(owners);
        for (m, uri) in [
            ("GET", "/v1/config/mcp-servers"),
            ("POST", "/v1/config/inference-profiles/p1/resolve"),
            ("GET", "/v1/config/catalog"),
        ] {
            assert_eq!(
                call(&app, m, uri, Some("tenant-b"), None).await,
                StatusCode::OK,
                "{m} {uri}"
            );
        }
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
        // A read never authors ownership…
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
