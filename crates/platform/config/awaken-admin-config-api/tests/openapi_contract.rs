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
        grant_renewal: None,
        probe: None,
        model_discovery: None,
        brokered_catalog: None,
        availability: Default::default(),
        clock: awaken_time::system_clock(),
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
fn config_read_workspace_claim_is_optional_in_the_public_contract() {
    // Causes: the edge has already resolved the Workspace and the caller may
    // omit or include a workspace_id claim. Effects: OpenAPI cannot require a
    // second selector; the live router test owns equal/conflicting claims.
    // R1 edge scope + omitted claim => documented valid request for all three
    // read routes. R2 an explicit claim remains accepted as an optional field.
    let doc = openapi_document();
    for path in [
        "/v1/config/provider-connections",
        "/v1/config/executable-models",
        "/v1/config/credentials",
    ] {
        let parameters = doc["paths"][path]["get"]["parameters"]
            .as_array()
            .expect("GET parameters");
        let workspace = parameters
            .iter()
            .find(|parameter| parameter["name"] == "workspace_id")
            .expect("workspace claim parameter");
        assert_eq!(workspace["required"], false, "{path}");
    }
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
        ("post", "/v1/config/credentials/{id}/renew"),
        ("get", "/v1/config/credentials/{id}/renewal"),
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

/// Architecture fitness function: no wire type in the **model-authoring** graph
/// accepts input it does not model.
///
/// An authored model selection is all qualifiers — provider, dialect, endpoint.
/// A key the reader has never heard of is therefore never decoration: accepting
/// and dropping it resolves a *different* model than the author asked for, with
/// a success status and no diagnostic. Serde says that in
/// `#[serde(deny_unknown_fields)]`; schemars projects it as
/// `additionalProperties: false`; this walk checks the projection, so the
/// property is asserted over the whole reachable graph rather than one struct
/// at a time.
///
/// The guard is needed per type because `deny_unknown_fields` does **not**
/// recurse into a field's own type — `ModelSelectionWire` carried it while the
/// `ModelTarget` it wraps did not, and every authored `endpoint`, written for
/// `endpoint_name`, was silently discarded.
///
/// Scope is the model-authoring graph deliberately. Secret-free response views
/// are not authored input, and several request bodies outside this graph still
/// permit unknown keys; widening the gate is a separate, reviewed change rather
/// than a side effect of this one.
#[test]
fn the_model_authoring_graph_models_every_field_it_accepts() {
    let doc = openapi_document();
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("schemas object");

    // A schema that names a field set (`properties`) must close it. A map-shaped
    // object (`additionalProperties: <schema>`, no `properties`) is a modelled
    // open map, not an unmodelled field, and is left alone.
    fn open_field_sets(node: &serde_json::Value, path: &str, found: &mut Vec<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if map.contains_key("properties")
                    && map.get("additionalProperties") != Some(&serde_json::Value::Bool(false))
                {
                    found.push(path.to_string());
                }
                for (key, value) in map {
                    open_field_sets(value, &format!("{path}/{key}"), found);
                }
            }
            serde_json::Value::Array(items) => {
                for (index, value) in items.iter().enumerate() {
                    open_field_sets(value, &format!("{path}[{index}]"), found);
                }
            }
            _ => {}
        }
    }

    let mut open = Vec::new();
    for root in ["ModelSelection", "ModelTarget"] {
        let schema = schemas
            .get(root)
            .unwrap_or_else(|| panic!("the authoring graph must export `{root}`"));
        open_field_sets(schema, root, &mut open);
    }
    assert!(
        open.is_empty(),
        "these authored schemas accept fields they do not model \
         (add #[serde(deny_unknown_fields)] to the Rust type): {open:?}"
    );
}
