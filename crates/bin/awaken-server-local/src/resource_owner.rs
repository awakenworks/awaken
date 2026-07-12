//! Tenant ownership for the id-addressed management config resources (ADR-0051):
//! MCP server defs (`/v1/config/mcp-servers/{id}`), inference profiles
//! (`/v1/config/inference-profiles/{id}`), and webhook subscriptions
//! (`/v1/config/webhook-subscriptions/{id}`, ADR-0048).
//!
//! These live behind the cross-crate `awaken-config-resolver` stores (keyed by id
//! only), and the config-layer admin crate cannot see the agents-layer edge scope.
//! So ownership is enforced here, at the assembly layer, by a small middleware that
//! wraps the admin router: it records the authoring scope of each id on a
//! successful `PUT`, and answers a cross-tenant `GET`/`PUT` with **404** (never
//! 403 — no existence disclosure). The scope is the edge-stamped [`WorkspaceScope`]
//! (from workspace path addressing or an API key), defaulting to the seeded scope
//! for a single-tenant deployment, so such a deployment never fences itself.
//!
//! The model catalog (providers/endpoints/offerings) is intentionally NOT covered:
//! it is org/deployment-level shared configuration, not a per-workspace resource.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_protocol_managed::WorkspaceScope;
use axum::Json;
use axum::body::Body;
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

// ── Memory stores (ADR-0053 / ADR-0051) ───────────────────────────────────────
//
// Memory stores (`/v1/memory_stores/{id}`, and the `/memories`, `/memory_versions`
// subresources) are id-addressed and, unlike the config resources above, minted
// *server-side* (the id is in the create RESPONSE, not the request path). So this
// sibling guard records the owning scope by reading the id out of a successful
// `POST /v1/memory_stores` body, and fences any later cross-tenant access to that
// store (or its memories/versions) with **404**. A single-tenant deployment resolves
// every request to [`DEFAULT_SCOPE`], so it never fences itself. A store created
// before the guard saw it (unrecorded) stays open — first-touch does not steal
// ownership. NOTE: the `GET /v1/memory_stores` *list* is not scoped here (it needs a
// scope-carrying store to filter, a follow-on); per-store access — the actual
// read/write hole — is fenced.

/// Fence cross-tenant access to a memory store and record ownership of a freshly
/// created one (from the minted id in the create response body).
pub async fn memory_store_ownership_guard(
    State(owners): State<ResourceOwners>,
    request: Request,
    next: Next,
) -> Response {
    let scope = request
        .extensions()
        .get::<WorkspaceScope>()
        .map(|w| w.0.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
    let path = request.uri().path();
    let method = request.method().clone();

    // Access to an existing store (by id, including its subresources): fence a
    // known owner other than this scope.
    if let Some(id) = memory_store_id(path) {
        let key = format!("memstore:{id}");
        if let Some(owner) = owners.owner(&key)
            && owner != scope
        {
            return not_found();
        }
        return next.run(request).await;
    }

    // Create (`POST /v1/memory_stores`, no id yet): run it, then record the minted id.
    let is_create = method == Method::POST && is_memory_stores_collection(path);
    if !is_create {
        return next.run(request).await;
    }
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    // Buffer the small JSON body to read the minted id, then rebuild the response.
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 1 << 20).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "memory store response").into_response();
        }
    };
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(id) = v.get("id").and_then(|i| i.as_str())
    {
        owners.record(format!("memstore:{id}"), scope);
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// The store id in a `/v1/memory_stores/{id}...` path (covers the bare store and all
/// its subresources), or `None` for the collection route and anything else.
fn memory_store_id(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "v1" || segments.next()? != "memory_stores" {
        return None;
    }
    segments
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Whether `path` is the `/v1/memory_stores` collection route (no id).
fn is_memory_stores_collection(path: &str) -> bool {
    let mut segments = path.trim_start_matches('/').split('/');
    segments.next() == Some("v1")
        && segments.next() == Some("memory_stores")
        && segments.next().is_none_or(str::is_empty)
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
    fn memory_store_id_covers_the_store_and_its_subresources() {
        assert_eq!(
            memory_store_id("/v1/memory_stores/memstore_1"),
            Some("memstore_1".to_string())
        );
        assert_eq!(
            memory_store_id("/v1/memory_stores/memstore_1/memories"),
            Some("memstore_1".to_string())
        );
        assert_eq!(
            memory_store_id("/v1/memory_stores/memstore_1/memories/mem_9"),
            Some("memstore_1".to_string())
        );
        assert_eq!(
            memory_store_id("/v1/memory_stores/memstore_1/memory_versions/v2/redact"),
            Some("memstore_1".to_string())
        );
        // The collection route (create/list) has no id.
        assert_eq!(memory_store_id("/v1/memory_stores"), None);
        assert_eq!(memory_store_id("/v1/files/f1"), None);
    }

    #[test]
    fn only_the_bare_collection_is_a_create_target() {
        assert!(is_memory_stores_collection("/v1/memory_stores"));
        assert!(is_memory_stores_collection("/v1/memory_stores/"));
        assert!(!is_memory_stores_collection("/v1/memory_stores/memstore_1"));
        assert!(!is_memory_stores_collection("/v1/files"));
    }

    #[test]
    fn owner_records_and_fences_by_scope() {
        let owners = ResourceOwners::new();
        owners.record("memstore:memstore_1".into(), "tenant-a".into());
        assert_eq!(
            owners.owner("memstore:memstore_1").as_deref(),
            Some("tenant-a")
        );
        // A different scope is a mismatch (the middleware turns this into a 404); an
        // unrecorded store has no owner (stays open).
        assert!(owners.owner("memstore:memstore_1") != Some("tenant-b".to_string()));
        assert_eq!(owners.owner("memstore:unknown"), None);
    }
}
