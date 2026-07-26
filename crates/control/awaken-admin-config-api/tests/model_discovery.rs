//! Provider-model discovery stays an application/provisioning collaboration:
//! HTTP selects one authored endpoint and exact Workspace credential, a fake
//! provisioning adapter returns a complete secret-free observation, and the
//! existing catalog repository atomically reconciles it.
//!
//! Provider connection cause/effect model (co-located with executable tests):
//!
//! - C1 installed provider+dialect, C2 non-empty key, C3 complete non-empty
//!   discovery => E1 secret-free active credential + E2 atomic Provider/Endpoint/
//!   Offerings + E5 no secret output (`test_and_save_activates…`, T1).
//! - C4 discovery failure => E3 no credential or catalog facts
//!   (`failed_connection_test…`, T4).
//! - C5 catalog rejects the observed list => E3 no catalog facts + E4 the
//!   sealed, pre-activation credential remains disabled (`catalog_rejection…`, T5).
//!
//! Decision table:
//!
//! | Rule | T1 | T2 unsupported | T3 empty key | T4 discovery | T5 catalog |
//! |---|---:|---:|---:|---:|---:|
//! | C1 descriptor match | 1 | 0 | 1 | 1 | 1 |
//! | C2 key present | 1 | 1 | 0 | 1 | 1 |
//! | C3 discovery success | 1 | - | - | 0 | 1 |
//! | E1 active secret-free credential | 1 | 0 | 0 | 0 | 0 |
//! | E2 atomic catalog visibility | 1 | 0 | 0 | 0 | 0 |
//! | E3 no executable catalog facts | 0 | 1 | 1 | 1 | 1 |
//! | E4 disabled credential | 0 | 0 | 0 | 0 | 1 |
//! | E5 no secret serialization | 1 | 1 | 1 | 1 | 1 |
//!
//! Read-model status graph (`GET provider-connections`): no authored facts ->
//! `not_configured`; active credential without offerings -> `connected`; active
//! credential + active fresh offering -> `ready`; same with last observation
//! older than TTL -> `stale`; configured without an active credential ->
//! `needs_attention`; active credential + only unavailable offerings ->
//! `unavailable`. Tests below exercise every leaf from composed stores.
//!
//! Brokered refresh decision table: B1 adapter present + valid projection ->
//! atomically expose only `brokered` offerings; B2 adapter absent -> typed 503
//! and no catalog mutation; B3 adapter failure -> typed 503 and no mutation.

use std::sync::{Arc, Mutex};

use awaken_admin_config_api::{
    AdminState, BrokeredCatalogDiscovery, ModelCatalogDiscovery, ModelCatalogDiscoveryError,
    admin_router,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{AvailabilityLedger, CredentialSource};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ApiDialect, BrokeredCatalogProjection, BrokeredModelProjection, DiscoveredModel,
    OfferingSource, OfferingStatus, ProtocolEndpoint,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct FixedDiscovery {
    models: Mutex<Vec<DiscoveredModel>>,
    calls: Mutex<Vec<(String, String)>>,
    fail: Mutex<bool>,
    secret_calls: Mutex<Vec<(String, String)>>,
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
        if *self.fail.lock().unwrap() {
            return Err(ModelCatalogDiscoveryError::Provider(
                "injected provider failure".into(),
            ));
        }
        Ok(self.models.lock().unwrap().clone())
    }

    async fn discover_with_secret(
        &self,
        endpoint: &ProtocolEndpoint,
        secret: &RedactedString,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        self.secret_calls
            .lock()
            .unwrap()
            .push((endpoint.id.0.clone(), secret.expose_secret().to_string()));
        if *self.fail.lock().unwrap() {
            return Err(ModelCatalogDiscoveryError::Provider(
                "injected provider failure".into(),
            ));
        }
        Ok(self.models.lock().unwrap().clone())
    }
}

struct Harness {
    app: Router,
    catalog: Arc<InMemoryCatalogRepo>,
    discovery: Arc<FixedDiscovery>,
    credentials: Arc<InMemoryCredentialRepo>,
}

struct FixedBrokeredDiscovery(Result<BrokeredCatalogProjection, String>);

#[async_trait::async_trait]
impl BrokeredCatalogDiscovery for FixedBrokeredDiscovery {
    async fn projection(&self) -> Result<BrokeredCatalogProjection, String> {
        self.0.clone()
    }
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
        fail: Mutex::new(false),
        secret_calls: Mutex::new(Vec::new()),
    });
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: credentials.clone(),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: Some(discovery.clone()),
        brokered_catalog: None,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    Harness {
        app,
        catalog,
        discovery,
        credentials,
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
async fn b1_brokered_refresh_exposes_only_explicit_managed_offerings() {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: None,
        brokered_catalog: Some(Arc::new(FixedBrokeredDiscovery(Ok(
            BrokeredCatalogProjection {
                broker_id: "awaken-cloud".into(),
                control_base_url: "https://api.awakenworks.com".into(),
                models: vec![BrokeredModelProjection {
                    provider_id: "openai".into(),
                    model_id: "gpt-5".into(),
                    dialect: ApiDialect::OpenAiResponses,
                    context_window: Some(400_000),
                    max_output_tokens: Some(128_000),
                    publication_revision: 7,
                }],
                observed_at_unix_ms: 42,
            },
        )))),
        availability: Arc::new(AvailabilityLedger::new()),
    });

    let (status, result) = call(
        &app,
        "POST",
        "/v1/config/brokered-models/refresh",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["activated"], 1);
    let snapshot = catalog.snapshot().await.unwrap();
    assert_eq!(snapshot.offerings.len(), 1);
    assert_eq!(snapshot.offerings[0].source, OfferingSource::Brokered);
    assert_eq!(
        snapshot.model_attributes["gpt-5"].context_window,
        Some(400_000)
    );
    assert_eq!(
        snapshot.model_attributes["gpt-5"].provenance["context_window"].source,
        awaken_model_catalog::ModelAttributeSource::Brokered
    );
    assert_eq!(
        snapshot.endpoints[&snapshot.offerings[0].protocol_endpoint_id.0]
            .base_url
            .as_deref(),
        Some("https://api.awakenworks.com")
    );
}

#[tokio::test]
async fn b2_missing_broker_adapter_is_typed_and_does_not_mutate_catalog() {
    let harness = harness();
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/brokered-models/refresh",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem["code"], "brokered_catalog_unavailable");
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
    let first_observed_at = result["observed_at_unix_ms"].as_u64().unwrap();
    assert!(first_observed_at > 0);
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
    let second_observed_at = result["observed_at_unix_ms"].as_u64().unwrap();
    assert!(second_observed_at >= first_observed_at);

    let catalog = harness.catalog.snapshot().await.unwrap();
    let stale = catalog
        .offerings
        .iter()
        .find(|offering| offering.model_id == "provider-a")
        .unwrap();
    assert_eq!(stale.source, OfferingSource::ProviderApi);
    assert_eq!(stale.status, OfferingStatus::Unavailable);
    assert_eq!(stale.last_seen_at_unix_ms, Some(first_observed_at));
    assert!(
        catalog
            .resolve_offering("provider-a", stale.dialect)
            .is_none()
    );
}

#[tokio::test]
async fn test_and_save_activates_all_facts_only_after_discovery_succeeds() {
    let harness = harness();
    let secret = "connection-test-secret"; // awaken-allow: secret -- inert fixture
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "endpoint_id":"anthropic-messages",
            "dialect":"anthropic_messages",
            "secret":secret
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["sync"]["discovered"], 2);
    assert_eq!(result["credential"]["status"], "active");
    assert_eq!(
        result["endpoint"]["base_url"],
        "https://api.anthropic.com/v1"
    );
    assert!(!result.to_string().contains(secret));
    assert_eq!(
        harness.discovery.secret_calls.lock().unwrap().as_slice(),
        &[("anthropic-messages".into(), secret.into())]
    );
    let catalog = harness.catalog.snapshot().await.unwrap();
    assert_eq!(catalog.providers.len(), 1);
    assert_eq!(catalog.endpoints.len(), 1);
    assert_eq!(catalog.offerings.len(), 2);
}

#[tokio::test]
async fn failed_connection_test_leaves_no_executable_catalog_facts() {
    let harness = harness();
    *harness.discovery.fail.lock().unwrap() = true;
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
            "endpoint_id":"openai-responses",
            "dialect":"open_ai_responses",
            "base_url":"https://api.openai.com/v1",
            "secret":"bad-key"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    let catalog = harness.catalog.snapshot().await.unwrap();
    assert!(catalog.providers.is_empty());
    assert!(catalog.endpoints.is_empty());
    assert!(catalog.offerings.is_empty());
}

#[tokio::test]
async fn unsupported_provider_and_empty_key_fail_before_discovery() {
    let harness = harness();
    let secret = "must-not-echo";
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a",
            "provider_id":"unknown-provider",
            "display_name":"Unknown",
            "endpoint_id":"unknown",
            "dialect":"open_ai_chat",
            "secret":secret
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(!problem.to_string().contains(secret));

    let (status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
            "endpoint_id":"openai-responses",
            "dialect":"open_ai_responses",
            "secret":""
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(harness.discovery.secret_calls.lock().unwrap().is_empty());
    assert!(
        harness
            .catalog
            .snapshot()
            .await
            .unwrap()
            .providers
            .is_empty()
    );
}

#[tokio::test]
async fn catalog_rejection_disables_the_already_sealed_credential() {
    let harness = harness();
    *harness.discovery.models.lock().unwrap() = vec![DiscoveredModel {
        model_id: " ".into(),
        upstream_model: None,
    }];
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "endpoint_id":"anthropic-messages",
            "dialect":"anthropic_messages",
            "base_url":"https://provider.invalid/v1",
            "secret":"sealed-before-catalog-rejection"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    let catalog = harness.catalog.snapshot().await.unwrap();
    assert!(catalog.providers.is_empty());
    assert!(catalog.endpoints.is_empty());
    assert!(catalog.offerings.is_empty());

    let sources = harness.credentials.list("workspace-a").await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0].status,
        awaken_credential_vault::CredentialStatus::Disabled
    );
}

fn summary<'a>(values: &'a Value, provider_id: &str) -> &'a Value {
    values
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["provider_id"] == provider_id)
        .unwrap()
}

#[tokio::test]
async fn connection_summaries_cover_not_configured_ready_stale_and_unavailable() {
    let harness = harness();
    let (_, initial) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&initial, "anthropic")["status"], "not_configured");

    let (status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "workspace_id":"workspace-a", "provider_id":"anthropic",
            "display_name":"Anthropic", "endpoint_id":"anthropic-messages",
            "dialect":"anthropic_messages", "secret":"summary-secret"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, ready) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&ready, "anthropic")["status"], "ready");
    assert_eq!(summary(&ready, "anthropic")["active_models"], 2);

    harness
        .catalog
        .reconcile_discovered_models(
            &awaken_model_catalog::ProtocolEndpointId::new("anthropic-messages"),
            vec![DiscoveredModel {
                model_id: "provider-a".into(),
                upstream_model: None,
            }],
            1,
        )
        .await
        .unwrap();
    let (_, stale) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&stale, "anthropic")["status"], "stale");

    harness
        .catalog
        .reconcile_discovered_models(
            &awaken_model_catalog::ProtocolEndpointId::new("anthropic-messages"),
            Vec::new(),
            2,
        )
        .await
        .unwrap();
    let (_, unavailable) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&unavailable, "anthropic")["status"], "unavailable");
}

#[tokio::test]
async fn connection_summaries_separate_connected_from_needs_attention() {
    let harness = harness();
    let (status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/credentials",
        json!({
            "workspace_id":"workspace-a", "kind":"vault", "provider_id":"openai",
            "secret":"credential-only-secret"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, connected) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&connected, "openai")["status"], "connected");

    assert_eq!(
        call(
            &harness.app,
            "PUT",
            "/v1/config/providers/gemini",
            json!({"id":"ignored", "slug":"gemini", "display_name":"Gemini", "version":1}),
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, attention) = call(
        &harness.app,
        "GET",
        "/v1/config/provider-connections?workspace_id=workspace-a",
        json!({}),
    )
    .await;
    assert_eq!(summary(&attention, "gemini")["status"], "needs_attention");
}

#[tokio::test]
async fn failed_refresh_does_not_advance_last_seen_or_change_availability() {
    let harness = harness();
    let credential_id = author_prerequisites(&harness.app).await;
    let request = json!({
        "workspace_id":"workspace-a",
        "credential_source_id": credential_id
    });
    let (status, first) = call(
        &harness.app,
        "POST",
        "/v1/config/endpoints/ep1/discover-models",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let before = harness.catalog.snapshot().await.unwrap();

    *harness.discovery.fail.lock().unwrap() = true;
    *harness.discovery.models.lock().unwrap() = Vec::new();
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/endpoints/ep1/discover-models",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["code"], "model_discovery_failed");
    assert_eq!(harness.catalog.snapshot().await.unwrap(), before);
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
