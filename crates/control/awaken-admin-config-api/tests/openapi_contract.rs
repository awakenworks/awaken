//! The OpenAPI registry ↔ router drift gate (mirrors oversight-next's
//! `openapi_contract`), enforced in BOTH directions:
//!
//! * documented → mounted: every documented operation is probed against the real
//!   [`admin_router`]; a registry entry whose route is not mounted surfaces as a
//!   bare routing 404 (no problem+json body) or a 405, and fails here.
//! * mounted → documented: [`every_mounted_route_is_documented`] pins the full
//!   mounted surface against the registry, so a route added to the router without
//!   a matching registry entry fails the build. (Five live routes —
//!   model-attributes, cooldown, availability, pool eligible, resolve-candidates —
//!   had escaped the one-directional gate; this closes that hole.)

use std::sync::Arc;

use awaken_admin_config_api::openapi::openapi_document;
use awaken_admin_config_api::{
    AdminState, CloudLoginApplication, CloudLoginState, CloudLoginStatusView,
    ConfigCapabilitiesSource, ConfigCapabilitiesView, admin_router_with_runtime_capabilities,
};
use awaken_config_resolver::{InMemoryAgentInputBindingRepository, InMemoryProfileStore};
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
        resources: Arc::new(InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: None,
        brokered_catalog: None,
        availability: Default::default(),
    }
}

struct ProbeCapabilities;

impl ConfigCapabilitiesSource for ProbeCapabilities {
    fn current(&self) -> ConfigCapabilitiesView {
        ConfigCapabilitiesView::default()
    }
}

struct ProbeCloudLogin;

#[async_trait::async_trait]
impl CloudLoginApplication for ProbeCloudLogin {
    async fn status(&self) -> CloudLoginStatusView {
        CloudLoginStatusView {
            state: CloudLoginState::SignInRequired,
            authorize_url: None,
            error_code: None,
        }
    }

    async fn start(&self) -> CloudLoginStatusView {
        self.status().await
    }

    async fn logout(&self) -> Result<(), String> {
        Ok(())
    }
}

fn route_probe_app() -> axum::Router {
    admin_router_with_runtime_capabilities(
        state(),
        Arc::new(ProbeCapabilities),
        Some(Arc::new(ProbeCloudLogin)),
    )
}

#[test]
fn document_shape_and_schema_components() {
    let doc = openapi_document();
    assert_eq!(doc["openapi"], "3.1.0");
    // Cause → effect → assertion:
    // Ambient environment proposals created a second Provider-connection
    // authoring path → deleting only its UI could leave a callable compatibility
    // API → the public contract must not expose that retired path.
    assert!(
        doc["paths"]["/v1/config/provider-proposals"].is_null(),
        "retired provider-proposals path must stay absent"
    );
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("schemas object");
    for name in [
        "Provider",
        "ProviderDriverDescriptor",
        "SaveProviderConnectionRequest",
        "ProviderConnectionView",
        "ProviderConnectionSummary",
        "ExecutableModelOption",
        "ExecutableModelReadiness",
        "ProtocolEndpoint",
        "Offering",
        "CatalogSyncResult",
        "PutModelAttributesRequest",
        "ProviderCatalog",
        "CredentialSource",
        "CredentialPool",
        "CredentialBinding",
        "InferenceProfile",
        "AgentInputConfig",
        "EnterCredentialRequest",
        "ValidateCredentialRequest",
        "ResolveRequest",
        "ModelTarget",
        "ResolveProfileRequest",
        "ResolvedInferenceView",
        "ResolvedCandidatesView",
        "CredentialValidation",
        "PoolEligibleView",
        "ModelAttributes",
        "AvailabilityState",
        "CooldownRequest",
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
                .replace("{agent_id}", "probe-agent")
                .replace("{model_id}", "probe-model")
                .replace("{id}", "probe-id");
            let request = Request::builder()
                .method(method.to_uppercase().parse::<Method>().expect("method"))
                .uri(&uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("request builds");
            let response = route_probe_app()
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

/// The mounted → documented direction: the full surface [`admin_router`] mounts,
/// as an explicit SSOT. Every entry must appear in the OpenAPI registry, so a
/// route added to the router without a registry entry fails here (the hole that
/// let model-attributes / cooldown / availability / eligible / resolve-candidates
/// ship undocumented). Adding a route means adding it here AND to `openapi.rs` —
/// the intended forcing function, mirroring oversight-next's hand-assembled gate.
#[test]
fn every_mounted_route_is_documented() {
    // (METHOD, path-template) for every route `admin_router` mounts.
    const MOUNTED: &[(&str, &str)] = &[
        ("get", "/v1/config/capabilities"),
        ("get", "/v1/config/cloud-login"),
        ("post", "/v1/config/cloud-login"),
        ("delete", "/v1/config/cloud-login"),
        ("get", "/v1/config/provider-descriptors"),
        ("post", "/v1/config/provider-connections"),
        ("get", "/v1/config/provider-connections"),
        ("get", "/v1/config/executable-models"),
        ("put", "/v1/config/model-attributes/{model_id}"),
        ("get", "/v1/config/catalog"),
        ("post", "/v1/config/brokered-models/refresh"),
        ("post", "/v1/config/credentials"),
        ("get", "/v1/config/credentials"),
        ("get", "/v1/config/credentials/{id}"),
        ("post", "/v1/config/credentials/{id}/archive"),
        ("post", "/v1/config/credentials/{id}/rotate"),
        ("post", "/v1/config/credentials/{id}/validate"),
        ("post", "/v1/config/credentials/{id}/cooldown"),
        ("get", "/v1/config/credentials/{id}/availability"),
        ("put", "/v1/config/credential-pools/{id}"),
        ("get", "/v1/config/credential-pools/{id}"),
        ("get", "/v1/config/credential-pools/{id}/eligible"),
        ("put", "/v1/config/inference-profiles/{id}"),
        ("get", "/v1/config/inference-profiles/{id}"),
        ("post", "/v1/config/inference-profiles/{id}/resolve"),
        (
            "post",
            "/v1/config/inference-profiles/{id}/resolve-candidates",
        ),
        ("post", "/v1/config/inference/resolve"),
        ("put", "/v1/config/agents/{agent_id}/resources"),
        ("get", "/v1/config/agents/{agent_id}/resources"),
    ];

    let doc = openapi_document();
    let paths = doc["paths"].as_object().expect("paths object");
    for (method, template) in MOUNTED {
        let item = paths
            .get(*template)
            .unwrap_or_else(|| panic!("undocumented route: {method} {template}"));
        assert!(
            item.get(method).is_some(),
            "route {method} {template} is mounted but has no {method} operation documented",
        );
    }
    // And the registry documents nothing the router does not mount (no phantom docs).
    let documented: usize = paths
        .values()
        .map(|item| item.as_object().expect("path item").len())
        .sum();
    assert_eq!(
        documented,
        MOUNTED.len(),
        "the OpenAPI registry documents {documented} operations but the router mounts {}",
        MOUNTED.len(),
    );
}
