//! Tenant ownership for the id-addressed memory stores (ADR-0053 / ADR-0051).
//!
//! Memory stores (`/v1/memory_stores/{id}`, and the `/memories`, `/memory_versions`
//! subresources) are id-addressed. The Resource Catalog owns the Workspace fence;
//! this middleware only applies that intrinsic invariant before subresource handlers.
//!
//! This is the data-plane sibling of the config-resource ownership guard, which lives
//! in the authoring plane (`awaken-control`). Scope selection belongs to the outer
//! composition/PEP layer; this module only consumes the selected Workspace and
//! checks the catalog invariant.

use std::sync::Arc;

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
pub struct MemoryStoreOwnershipLookup(Arc<dyn ResourceCatalog>);

impl MemoryStoreOwnershipLookup {
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
    State(owners): State<MemoryStoreOwnershipLookup>,
    request: Request,
    next: Next,
) -> Response {
    let Some(scope) = request_scope(&request) else {
        return not_found();
    };
    let path = request.uri().path().to_string();
    if let Some(id) = memory_store_id(&path) {
        if !owners.owns(scope, &id) {
            return not_found();
        }
        return next.run(request).await;
    }
    next.run(request).await
}

fn request_scope(request: &Request) -> Option<&str> {
    request
        .extensions()
        .get::<WorkspaceScope>()
        .map(|workspace| workspace.0.as_str())
        .filter(|workspace| !workspace.trim().is_empty())
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
    fn missing_and_empty_scope_are_rejected_without_a_local_fallback() {
        let request = Request::new(Body::empty());
        assert_eq!(request_scope(&request), None);

        let mut request = Request::new(Body::empty());
        request
            .extensions_mut()
            .insert(WorkspaceScope(String::new()));
        assert_eq!(request_scope(&request), None);

        let mut request = Request::new(Body::empty());
        request
            .extensions_mut()
            .insert(WorkspaceScope("workspace-a".into()));
        assert_eq!(request_scope(&request), Some("workspace-a"));
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
        let owners = MemoryStoreOwnershipLookup::over(catalog);
        assert!(owners.owns("tenant-a", "memstore_1"));
        assert!(!owners.owns("tenant-b", "memstore_1"));
        assert!(!owners.owns("tenant-a", "unknown"));
    }
}
