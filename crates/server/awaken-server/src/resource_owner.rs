//! Tenant ownership for the id-addressed memory stores (ADR-0053 / ADR-0051).
//!
//! Memory stores (`/v1/memory_stores/{id}`, and the `/memories`, `/memory_versions`
//! subresources) are id-addressed and minted *server-side* (the id is in the create
//! RESPONSE, not the request path). So this guard records the owning scope by reading
//! the id out of a successful `POST /v1/memory_stores` body, fences any later
//! cross-tenant access to that store (or its memories/versions) with **404**, and
//! filters the store *list* to the caller's own scope so ids do not leak across
//! tenants. A single-tenant deployment resolves every request to [`DEFAULT_SCOPE`], so
//! it never fences itself. A store created before the guard saw it (unrecorded) stays
//! reachable by direct id — first-touch does not steal ownership — but an unrecorded
//! store belongs to no scope, so it never appears in a scoped list.
//!
//! This is the data-plane sibling of the config-resource ownership guard, which lives
//! in the authoring plane (`awaken-control`); the two guards are independent (each
//! owns its own [`ResourceOwners`] instance).

use std::sync::Arc;

use awaken_config_resolver::{InMemoryMemoryStoreRegistry, MemoryStoreRegistry};
use awaken_config_store::DEFAULT_SCOPE;
use awaken_protocol_managed::WorkspaceScope;
use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Durable ownership adapter over the memory-store identity registry. Ownership
/// is aggregate state, not middleware cache, so it survives restarts and is shared
/// by every process using the same repository.
#[derive(Clone)]
pub struct ResourceOwners(Arc<dyn MemoryStoreRegistry>);

impl Default for ResourceOwners {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceOwners {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(InMemoryMemoryStoreRegistry::new()))
    }

    #[must_use]
    pub fn over(registry: Arc<dyn MemoryStoreRegistry>) -> Self {
        Self(registry)
    }

    fn owner(&self, key: &str) -> Option<String> {
        let id = key.strip_prefix("memstore:")?;
        self.0
            .get_memory_store(id)
            .map(|def| def.workspace_id)
            .filter(|workspace| !workspace.is_empty())
    }

    fn record(&self, key: String, scope: String) {
        let Some(id) = key.strip_prefix("memstore:") else {
            return;
        };
        if let Some(mut def) = self.0.get_memory_store(id) {
            def.workspace_id = scope;
            self.0.put_memory_store(def);
        }
    }
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

/// Fence cross-tenant access to a memory store, record ownership of a freshly created
/// one (from the minted id in the create response body), and scope the store list.
pub async fn memory_store_ownership_guard(
    State(owners): State<ResourceOwners>,
    mut request: Request,
    next: Next,
) -> Response {
    let scope = request_scope(&request);
    if scope.is_empty() {
        return not_found();
    }
    if request.extensions().get::<WorkspaceScope>().is_none() {
        request
            .extensions_mut()
            .insert(WorkspaceScope(scope.clone()));
    }
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // Access to an existing store (by id, including its subresources): fence a
    // known owner other than this scope.
    if let Some(id) = memory_store_id(&path) {
        let key = format!("memstore:{id}");
        if owners.owner(&key).as_deref() != Some(&scope) {
            return not_found();
        }
        return next.run(request).await;
    }

    // Only the bare `/v1/memory_stores` collection route remains.
    if !is_memory_stores_collection(&path) {
        return next.run(request).await;
    }

    // List (`GET`): drop stores this scope does not own, so ids do not leak.
    if method == Method::GET {
        let response = next.run(request).await;
        if !response.status().is_success() {
            return response;
        }
        return filter_store_list(response, &owners, &scope).await;
    }

    // Create (`POST`): run it, then record the minted id's owning scope.
    if method == Method::POST {
        let response = next.run(request).await;
        if !response.status().is_success() {
            return response;
        }
        let (parts, body) = response.into_parts();
        let bytes = match axum::body::to_bytes(body, 1 << 20).await {
            Ok(b) => b,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "memory store response")
                    .into_response();
            }
        };
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(id) = v.get("id").and_then(|i| i.as_str())
        {
            owners.record(format!("memstore:{id}"), scope);
        }
        return Response::from_parts(parts, Body::from(bytes));
    }

    next.run(request).await
}

fn request_scope(request: &Request) -> String {
    request
        .extensions()
        .get::<WorkspaceScope>()
        .map(|w| w.0.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string())
}

/// Rewrite a store-list response, keeping only the stores `scope` owns. On an
/// unparseable body (never, for our own shape) the response passes through unchanged.
async fn filter_store_list(response: Response, owners: &ResourceOwners, scope: &str) -> Response {
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 8 << 20).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "memory store list").into_response(),
    };
    let Ok(mut page) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some(data) = page.get_mut("data").and_then(|d| d.as_array_mut()) {
        data.retain(|store| {
            store
                .get("id")
                .and_then(|i| i.as_str())
                .is_some_and(|id| owners.owner(&format!("memstore:{id}")).as_deref() == Some(scope))
        });
    }
    // Rebuild with a fresh body/headers (the original content-length no longer holds).
    Json(page).into_response()
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
    fn absent_scope_uses_the_single_tenant_default_but_empty_scope_is_rejected() {
        let request = Request::new(Body::empty());
        assert_eq!(request_scope(&request), DEFAULT_SCOPE);

        let mut request = Request::new(Body::empty());
        request
            .extensions_mut()
            .insert(WorkspaceScope(String::new()));
        assert!(request_scope(&request).is_empty());
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
        let registry = Arc::new(InMemoryMemoryStoreRegistry::new());
        registry.put_memory_store(awaken_config_resolver::MemoryStoreDef {
            id: "memstore_1".into(),
            workspace_id: String::new(),
            name: String::new(),
            description: String::new(),
            metadata: Default::default(),
            archived: false,
        });
        let owners = ResourceOwners::over(registry);
        owners.record("memstore:memstore_1".into(), "tenant-a".into());
        assert_eq!(
            owners.owner("memstore:memstore_1").as_deref(),
            Some("tenant-a")
        );
        // A different scope is a mismatch (the middleware turns this into a 404);
        // an unrecorded/legacy store has no owner and is denied.
        assert!(owners.owner("memstore:memstore_1") != Some("tenant-b".to_string()));
        assert_eq!(owners.owner("memstore:unknown"), None);
    }
}
