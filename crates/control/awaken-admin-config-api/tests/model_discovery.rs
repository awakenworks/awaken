//! Provider-model discovery stays an application/provisioning collaboration:
//! HTTP selects one authored endpoint and exact Workspace credential, a fake
//! provisioning adapter returns a complete secret-free observation, and the
//! existing catalog repository atomically reconciles it.

use std::sync::{Arc, Mutex};

use awaken_admin_config_api::{
    AdminState, ModelCatalogDiscovery, ModelCatalogDiscoveryError, admin_router,
};
use awaken_credential_vault::{AvailabilityLedger, CredentialSource};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{DiscoveredModel, OfferingSource, OfferingStatus, ProtocolEndpoint};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct FixedDiscovery {
    models: Mutex<Vec<DiscoveredModel>>,
    calls: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl ModelCatalogDiscovery for FixedDiscovery {
    async fn discover(
        &self,
        endpoint: &ProtocolEndpoint,
        credential: &CredentialSource,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        self.calls
            .lock()
            .unwrap()
            .push((endpoint.id.0.clone(), credential.id.0.clone()));
        Ok(self.models.lock().unwrap().clone())
    }
}

struct Harness {
    app: Router,
    catalog: Arc<InMemoryCatalogRepo>,
    discovery: Arc<FixedDiscovery>,
}

fn harness() -> Harness {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let discovery = Arc::new(FixedDiscovery {
        models: Mutex::new(vec![
            DiscoveredModel {
                model_id: "provider-a".into(),
                upstream_model: None,
            },
            DiscoveredModel {
                model_id: "provider-b".into(),
                upstream_model: None,
            },
        ]),
        calls: Mutex::new(Vec::new()),
    });
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: Some(discovery.clone()),
        availability: Arc::new(AvailabilityLedger::new()),
    });
    Harness {
        app,
        catalog,
        discovery,
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn author_prerequisites(app: &Router) -> String {
    assert_eq!(
        call(
            app,
            "PUT",
            "/v1/config/providers/anthropic",
            json!({"id":"ignored", "slug":"anthropic", "display_name":"Anthropic", "version":1}),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app,
            "PUT",
            "/v1/config/endpoints/ep1",
            json!({
                "id":"ignored", "provider_id":"anthropic", "dialect":"anthropic_messages",
                "base_url":"https://provider.invalid/v1", "timeout_secs":30,
                "display_name":"Anthropic", "version":1
            }),
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, credential) = call(
        app,
        "POST",
        "/v1/config/credentials",
        json!({
            "workspace_id":"workspace-a", "kind":"vault", "provider_id":"anthropic",
            "secret":"test-discovery-secret" // awaken-allow: secret -- inert in-memory fixture
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    credential["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn discovery_reconciles_through_the_existing_catalog_truth() {
    let harness = harness();
    let credential_id = author_prerequisites(&harness.app).await;
    let request = json!({
        "workspace_id":"workspace-a",
        "credential_source_id": credential_id
    });
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/endpoints/ep1/discover-models",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["discovered"], 2);
    assert_eq!(result["activated"], 2);
    assert_eq!(harness.discovery.calls.lock().unwrap().len(), 1);

    *harness.discovery.models.lock().unwrap() = vec![DiscoveredModel {
        model_id: "provider-b".into(),
        upstream_model: Some("provider-b-latest".into()),
    }];
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/endpoints/ep1/discover-models",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["marked_unavailable"], 1);

    let catalog = harness.catalog.snapshot().await.unwrap();
    let stale = catalog
        .offerings
        .iter()
        .find(|offering| offering.model_id == "provider-a")
        .unwrap();
    assert_eq!(stale.source, OfferingSource::ProviderApi);
    assert_eq!(stale.status, OfferingStatus::Unavailable);
    assert!(
        catalog
            .resolve_offering("provider-a", stale.dialect)
            .is_none()
    );
}

#[tokio::test]
async fn discovery_fails_closed_across_workspace() {
    let harness = harness();
    let credential_id = author_prerequisites(&harness.app).await;
    let (status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/endpoints/ep1/discover-models",
        json!({"workspace_id":"workspace-b", "credential_source_id":credential_id}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        harness
            .catalog
            .snapshot()
            .await
            .unwrap()
            .offerings
            .is_empty()
    );
}
