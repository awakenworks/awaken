//! The OpenAPI registry ↔ router drift gate (mirrors oversight-next's
//! `openapi_contract`). Every documented operation is probed against the real
//! [`admin_router`]: a registry entry whose route is not mounted surfaces as a
//! bare routing 404 (no problem+json body) or a 405, and fails here. The
//! reverse direction (mounted but undocumented) is caught in review — adding a
//! route without a registry entry leaves `contracts/openapi.json` stale, which
//! `generate-contracts.sh --check` flags in CI.

use std::sync::Arc;

use awaken_admin_config_api::openapi::openapi_document;
use awaken_admin_config_api::{
    AdminState, InMemoryMcpStore, InMemoryProfileStore, InMemoryResourceStore, admin_router,
};
use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_model_catalog::repo::InMemoryCatalogRepo;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header::CONTENT_TYPE};
use tower::ServiceExt;

fn state() -> AdminState {
    AdminState {
        catalog: Arc::new(InMemoryCatalogRepo::new()),
        credentials: Arc::new(InMemoryCredentialRepo::new()),
        secrets: Arc::new(InMemorySecretStore::new()),
        profiles: Arc::new(InMemoryProfileStore::new()),
        mcp: Arc::new(InMemoryMcpStore::new()),
        resources: Arc::new(InMemoryResourceStore::new()),
        probe: None,
        availability: Default::default(),
    }
}

#[test]
fn document_shape_and_schema_components() {
    let doc = openapi_document();
    assert_eq!(doc["openapi"], "3.1.0");
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("schemas object");
    for name in [
        "Provider",
        "ProtocolEndpoint",
        "Offering",
        "ProviderCatalog",
        "CredentialSource",
        "CredentialPool",
        "CredentialBinding",
        "InferenceProfile",
        "McpServerDef",
        "AgentMcpConfig",
        "AgentResourceConfig",
        "EnterCredentialRequest",
        "ValidateCredentialRequest",
        "ResolveRequest",
        "ResolveProfileRequest",
        "ResolveAgentMcpRequest",
        "ResolvedInferenceView",
        "ResolvedMcpServerView",
        "CredentialValidation",
        "ApiError",
    ] {
        assert!(schemas.contains_key(name), "missing schema `{name}`");
    }
    // Every $ref in the paths object must point at a present component.
    let paths = serde_json::to_string(&doc["paths"]).expect("paths serialize");
    for reference in paths
        .split("#/components/schemas/")
        .skip(1)
        .map(|rest| rest.split('"').next().expect("ref terminates"))
    {
        assert!(
            schemas.contains_key(reference),
            "dangling $ref `{reference}`"
        );
    }
}

/// A routing miss is a bare 404 (no problem+json body); a domain 404 speaks
/// RFC 9457. Probing each documented (path, method) with placeholder ids
/// therefore distinguishes "mounted" from "documented but absent".
#[tokio::test]
async fn every_documented_operation_is_mounted() {
    let doc = openapi_document();
    let paths = doc["paths"].as_object().expect("paths object");
    for (template, item) in paths {
        for (method, _) in item.as_object().expect("path item object") {
            let uri = template
                .replace("{project_id}", "probe-project")
                .replace("{agent_id}", "probe-agent")
                .replace("{id}", "probe-id");
            let request = Request::builder()
                .method(method.to_uppercase().parse::<Method>().expect("method"))
                .uri(&uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("request builds");
            let response = admin_router(state())
                .oneshot(request)
                .await
                .expect("router responds");
            let status = response.status();
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {template}: documented method not mounted"
            );
            if status == StatusCode::NOT_FOUND {
                let content_type = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                assert_eq!(
                    content_type, "application/problem+json",
                    "{method} {template}: routing 404 — documented route not mounted"
                );
            }
        }
    }
}

/// The `openapi_contract` gate only catches *documented-but-unmounted*; the reverse
/// (mounted-but-undocumented) is a review concern. This pins one such omission that
/// is live today so it is not lost: `resolve_profile_candidates_route` is mounted by
/// `admin_router` at `.../resolve-candidates` but has no `openapi::paths()` entry,
/// so it is absent from the emitted contract and never drift-probed above.
///
// KNOWN GAP (adjudicate): resolve_profile_candidates_route absent from openapi paths()
#[test]
fn resolve_candidates_route_is_undocumented_known_gap() {
    let doc = openapi_document();
    let paths = doc["paths"].as_object().expect("paths object");
    assert!(
        !paths.contains_key("/v1/config/inference-profiles/{id}/resolve-candidates"),
        "resolve-candidates is now documented — adjudicate the KNOWN GAP and fold it \
         into the mounted-operation drift assertions above"
    );
}
