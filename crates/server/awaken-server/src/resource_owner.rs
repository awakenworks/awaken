//! Tenant ownership for the id-addressed memory stores (ADR-0053 / ADR-0051).
//!
//! Memory stores (`/v1/memory_stores/{id}`, and the `/memories`, `/memory_versions`
//! subresources) are id-addressed. The Resource Catalog owns the Workspace fence;
//! this middleware only applies that intrinsic invariant before subresource handlers.
//!
//! This is the data-plane sibling of the config-resource ownership guard, which lives
//! in the authoring plane (`awaken-control`); the two guards are independent (each
//! owns its own [`ResourceOwners`] instance).

use std::sync::Arc;

use awaken_config_store::DEFAULT_SCOPE;
use awaken_protocol_managed::{ResourceCatalog, WorkspaceScope};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Intrinsic ownership adapter over the resource catalog. Ownership is aggregate
/// state, not middleware cache, so it survives restarts and is shared by every
/// process using the same repository. This is not an authorization policy engine.
#[derive(Clone)]
pub struct ResourceOwners(Arc<dyn ResourceCatalog>);

impl ResourceOwners {
    #[must_use]
    pub fn over(catalog: Arc<dyn ResourceCatalog>) -> Self {
        Self(catalog)
    }

    fn owns(&self, scope: &str, id: &str) -> bool {
        self.0.memory_store(scope, id).is_some()
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

/// Fence cross-tenant access to a memory store. Collection create/list handlers
/// receive the trusted Workspace and query/write the same Catalog directly.
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
    if let Some(id) = memory_store_id(&path) {
        if !owners.owns(&scope, &id) {
            return not_found();
        }
        return next.run(request).await;
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
#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::InMemoryResourceCatalog;
    use awaken_protocol_managed::resource_plane::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceState,
    };
    use axum::body::Body;

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
    fn catalog_ownership_fences_by_scope() {
        let catalog = Arc::new(InMemoryResourceCatalog::new());
        catalog
            .create_memory_store(
                MemoryStoreDefinition {
                    id: "memstore_1".into(),
                    workspace_id: "tenant-a".into(),
                    name: String::new(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                },
                MemoryStoreConfigVersion {
                    memory_store_id: "memstore_1".into(),
                    version: ConfigVersion::INITIAL,
                    recall_policy: Default::default(),
                    extraction_policy: Default::default(),
                    retention_policy: Default::default(),
                },
            )
            .unwrap();
        let owners = ResourceOwners::over(catalog);
        assert!(owners.owns("tenant-a", "memstore_1"));
        assert!(!owners.owns("tenant-b", "memstore_1"));
        assert!(!owners.owns("tenant-a", "unknown"));
    }
}
