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
}
