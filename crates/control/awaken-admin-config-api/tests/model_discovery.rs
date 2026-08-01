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
//! Brokered refresh decision table: B1 feature+adapter+valid projection ->
//! atomically expose only `brokered` offerings; B2 feature disabled -> typed 409;
//! B3 enabled without login adapter -> typed 401; adapter failure -> typed 503.

use std::sync::{Arc, Mutex};

use awaken_admin_config_api::{
    AdminState, BrokeredCatalogDiscovery, ConfigCapabilitiesView, IdentityCapabilityView,
    ModelCatalogDiscovery, ModelCatalogDiscoveryError, ModelSupplyCapabilityView, admin_router,
    admin_router_with_capabilities,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{AvailabilityLedger, CredentialSource};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{
    ApiDialect, BrokeredCatalogProjection, BrokeredModelProjection, DiscoveredModel,
    OfferingSource, OfferingStatus, ProtocolEndpoint, Provider, ProviderId,
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

fn cloud_capabilities(authenticated: bool) -> ConfigCapabilitiesView {
    ConfigCapabilitiesView {
        identity: IdentityCapabilityView {
            mode: "awaken-cloud".into(),
            cloud_login_enabled: true,
            authenticated,
        },
        models: ModelSupplyCapabilityView {
            local_catalog_enabled: true,
            byok_enabled: true,
            cloud_models_enabled: true,
            profile_authoring_enabled: true,
        },
    }
}

fn hosted_capabilities() -> ConfigCapabilitiesView {
    ConfigCapabilitiesView {
        identity: IdentityCapabilityView {
            mode: "awaken-cloud".into(),
            cloud_login_enabled: false,
            authenticated: true,
        },
        models: ModelSupplyCapabilityView {
            local_catalog_enabled: false,
            byok_enabled: false,
            cloud_models_enabled: true,
            profile_authoring_enabled: false,
        },
    }
}

#[async_trait::async_trait]
impl BrokeredCatalogDiscovery for FixedBrokeredDiscovery {
    async fn projection(&self) -> Result<BrokeredCatalogProjection, String> {
        self.0.clone()
    }
}

fn harness() -> Harness {
    harness_with_capabilities(ConfigCapabilitiesView::default())
}

fn harness_with_capabilities(capabilities: ConfigCapabilitiesView) -> Harness {
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
    let app = admin_router_with_capabilities(
        AdminState {
            catalog: catalog.clone(),
            credentials: credentials.clone(),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
            resources: Arc::new(
                awaken_admin_config_api::InMemoryAgentInputBindingRepository::new(),
            ),
            probe: None,
            model_discovery: Some(discovery.clone()),
            brokered_catalog: None,
            availability: Arc::new(AvailabilityLedger::new()),
        },
        capabilities,
    );
    Harness {
        app,
        catalog,
        discovery,
        credentials,
    }
}

#[tokio::test]
async fn hosted_supply_rejects_every_model_authoring_entry_before_side_effects() {
    // Cause-effect graph: hosted posture (C1) + a model-supply mutation (C2)
    // -> one typed denial (E1), no discovery/secret/catalog/profile side effect
    // (E2), while the read-only capability projection remains available (E3).
    //
    // Decision table:
    // | Rule | hosted | operation | E1 403 | E2 unchanged |
    // | H1 | yes | Provider connection | yes | yes |
    // | H2 | yes | Provider credential | yes | yes |
    // | H3 | yes | manual model attributes | yes | yes |
    // | H4 | yes | inference Profile | yes | yes |
    // | H5 | yes | manual brokered refresh | yes | yes |
    // Local authoring success is covered by the Provider-connection and B1
    // cases below, so these rules vary only the hosted posture and command.
    let harness = harness_with_capabilities(hosted_capabilities());
    let cases = [
        (
            "POST",
            "/v1/config/provider-connections",
            json!({
                "idempotency_key":"hosted-provider",
                "workspace_id":"workspace-a",
                "provider_id":"anthropic",
                "display_name":"Anthropic",
                "dialect":"anthropic_messages",
                "secret":"must-not-be-read" // awaken-allow: secret -- inert denial fixture
            }),
        ),
        (
            "POST",
            "/v1/config/credentials",
            json!({
                "workspace_id":"workspace-a",
                "kind":"vault",
                "provider_id":"anthropic",
                "secret":"must-not-be-stored" // awaken-allow: secret -- inert denial fixture
            }),
        ),
        (
            "PUT",
            "/v1/config/model-attributes/claude-native",
            json!({"context_window": 200000}),
        ),
        (
            "PUT",
            "/v1/config/inference-profiles/default",
            json!({
                "workspace_id":"workspace-a",
                "primary":{"target":{"model_id":"claude-native"},"credential_binding":{"type":"none"}},
                "fallbacks":[]
            }),
        ),
        ("POST", "/v1/config/brokered-models/refresh", Value::Null),
    ];
    for (method, path, body) in cases {
        let (status, problem) = call(&harness.app, method, path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {problem}");
        assert_eq!(problem["code"], "model_supply_managed", "{path}");
    }
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
    assert!(
        harness
            .credentials
            .list("workspace-a")
            .await
            .unwrap()
            .is_empty()
    );
    let (status, capabilities) =
        call(&harness.app, "GET", "/v1/config/capabilities", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["models"]["byok_enabled"], false);
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

fn connection_request(workspace_id: &str, credential_source_id: &str) -> Value {
    json!({
        "idempotency_key": "test-existing-command",
        "workspace_id": workspace_id,
        "provider_id": "anthropic",
        "display_name": "Anthropic",
        "dialect": "anthropic_messages",
        "base_url": "https://provider.invalid/v1",
        "timeout_secs": 30,
        "credential_source_id": credential_source_id
    })
}

#[tokio::test]
async fn b1_brokered_refresh_exposes_only_explicit_managed_offerings() {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let app = admin_router_with_capabilities(
        AdminState {
            catalog: catalog.clone(),
            credentials: Arc::new(InMemoryCredentialRepo::new()),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
            resources: Arc::new(
                awaken_admin_config_api::InMemoryAgentInputBindingRepository::new(),
            ),
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
        },
        cloud_capabilities(true),
    );

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

    // Turning the feature off preserves the durable projection for audit but
    // makes its API view unavailable, so UI/Profile clients cannot select it.
    let local = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        model_discovery: None,
        brokered_catalog: None,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    let (status, local_catalog) = call(&local, "GET", "/v1/config/catalog", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(local_catalog["offerings"][0]["status"], "unavailable");
    assert_eq!(
        catalog.snapshot().await.unwrap().offerings[0].status,
        OfferingStatus::Active,
        "feature projection must not destroy durable Cloud history"
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
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(problem["code"], "cloud_models_disabled");
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
async fn b3_enabled_cloud_supply_without_login_is_typed_and_non_mutating() {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let app = admin_router_with_capabilities(
        AdminState {
            catalog: catalog.clone(),
            credentials: Arc::new(InMemoryCredentialRepo::new()),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
            resources: Arc::new(
                awaken_admin_config_api::InMemoryAgentInputBindingRepository::new(),
            ),
            probe: None,
            model_discovery: None,
            brokered_catalog: None,
            availability: Arc::new(AvailabilityLedger::new()),
        },
        cloud_capabilities(false),
    );
    let (status, problem) = call(
        &app,
        "POST",
        "/v1/config/brokered-models/refresh",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(problem["code"], "cloud_sign_in_required");
    assert!(catalog.snapshot().await.unwrap().offerings.is_empty());
    let (status, capabilities) = call(&app, "GET", "/v1/config/capabilities", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["identity"]["cloud_login_enabled"], true);
    assert_eq!(capabilities["identity"]["authenticated"], false);
    assert_eq!(capabilities["models"]["cloud_models_enabled"], true);
}

#[tokio::test]
async fn discovery_reconciles_through_the_existing_catalog_truth() {
    let harness = harness();
    let credential_id = author_prerequisites(&harness.app).await;
    let request = connection_request("workspace-a", &credential_id);
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["sync"]["discovered"], 2);
    assert_eq!(result["sync"]["activated"], 2);
    let first_observed_at = result["sync"]["observed_at_unix_ms"].as_u64().unwrap();
    assert!(first_observed_at > 0);
    assert_eq!(harness.discovery.calls.lock().unwrap().len(), 1);

    *harness.discovery.models.lock().unwrap() = vec![DiscoveredModel {
        model_id: "provider-b".into(),
        upstream_model: Some("provider-b-latest".into()),
    }];
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["sync"]["marked_unavailable"], 1);
    let second_observed_at = result["sync"]["observed_at_unix_ms"].as_u64().unwrap();
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
            "idempotency_key":"test-api-key-command",
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "secret":secret
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["sync"]["discovered"], 2);
    assert_eq!(result["credential"]["status"], "active");
    assert_eq!(result["endpoint"]["id"], "anthropic.anthropic_messages");
    assert_eq!(
        result["endpoint"]["base_url"],
        "https://api.anthropic.com/v1"
    );
    assert!(!result.to_string().contains(secret));
    assert_eq!(
        harness.discovery.secret_calls.lock().unwrap().as_slice(),
        &[("anthropic.anthropic_messages".into(), secret.into())]
    );
    let catalog = harness.catalog.snapshot().await.unwrap();
    assert_eq!(catalog.providers.len(), 1);
    assert_eq!(catalog.endpoints.len(), 1);
    assert_eq!(catalog.offerings.len(), 2);
}

#[tokio::test]
async fn provider_and_dialect_are_the_only_authored_endpoint_identity() {
    // Cause/effect decision table:
    // R1 provider + dialect, no legacy endpoint_id -> canonical surface id;
    // R2 same provider, another dialect -> a distinct canonical surface id;
    // R3 same provider + dialect + endpoint_name -> a distinct named surface;
    // R4 same provider + dialect, arbitrary legacy endpoint_id -> the unnamed
    // canonical surface is updated, never a parallel client-named endpoint;
    // R5 invalid endpoint_name -> reject before discovery or persistence.
    let harness = harness();
    let request =
        |key: &str, dialect: &str, endpoint_name: Option<&str>, endpoint_id: Option<&str>| {
            let mut request = json!({
                "idempotency_key": key,
                "workspace_id":"workspace-a",
                "provider_id":"openai",
                "display_name":"OpenAI",
                "dialect":dialect,
                "secret":"surface-fixture" // awaken-allow: secret -- inert fixture
            });
            if let Some(endpoint_id) = endpoint_id {
                request["endpoint_id"] = json!(endpoint_id);
            }
            if let Some(endpoint_name) = endpoint_name {
                request["endpoint_name"] = json!(endpoint_name);
            }
            request
        };

    let (responses_status, responses) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("responses", "open_ai_responses", None, None),
    )
    .await;
    let (chat_status, chat) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("chat", "open_ai_chat", None, None),
    )
    .await;
    let (named_chat_status, named_chat) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("chat-regional", "open_ai_chat", Some("regional"), None),
    )
    .await;
    let (legacy_chat_status, legacy_chat) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("chat-legacy", "open_ai_chat", None, Some("client-invented")),
    )
    .await;
    let discovery_calls_before_invalid = harness.discovery.secret_calls.lock().unwrap().len();
    let (invalid_status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("chat-invalid", "open_ai_chat", Some("not/a/name"), None),
    )
    .await;

    assert_eq!(responses_status, StatusCode::CREATED, "{responses}");
    assert_eq!(chat_status, StatusCode::CREATED, "{chat}");
    assert_eq!(named_chat_status, StatusCode::CREATED, "{named_chat}");
    assert_eq!(legacy_chat_status, StatusCode::CREATED, "{legacy_chat}");
    assert_eq!(invalid_status, StatusCode::UNPROCESSABLE_ENTITY, "R5");
    assert_eq!(
        responses["endpoint"]["id"], "openai.open_ai_responses",
        "R1"
    );
    assert_eq!(chat["endpoint"]["id"], "openai.open_ai_chat", "R2");
    assert_eq!(
        chat["credential"]["protocol_endpoint_id"], "openai.open_ai_chat",
        "the connection credential is scoped to the endpoint it proved"
    );
    assert_eq!(
        named_chat["endpoint"]["id"], "openai.open_ai_chat.regional",
        "R3"
    );
    assert_eq!(legacy_chat["endpoint"]["id"], "openai.open_ai_chat", "R4");
    assert_eq!(
        harness.discovery.secret_calls.lock().unwrap().len(),
        discovery_calls_before_invalid,
        "R5"
    );
    let catalog = harness.catalog.snapshot().await.unwrap();
    assert_eq!(catalog.endpoints.len(), 3, "R2+R3+R4");
}

// Provider-command identity decision table:
// R1 same Workspace/provider/dialect/name/key replay -> same credential source.
// R2 same connection with a different key -> distinct credential source.
// R3 blank key -> reject before discovery or persistence.
#[tokio::test]
async fn provider_connection_command_is_idempotent_and_requires_an_explicit_key() {
    let harness = harness();
    let request = |key: &str| {
        json!({
            "idempotency_key": key,
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "secret":"idempotent-fixture" // awaken-allow: secret -- inert fixture
        })
    };

    let (first_status, first) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("command-a"),
    )
    .await;
    let (replay_status, replay) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("command-a"),
    )
    .await;
    let (second_status, second) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request("command-b"),
    )
    .await;
    let (blank_status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request(""),
    )
    .await;

    assert_eq!(first_status, StatusCode::CREATED);
    assert_eq!(replay_status, StatusCode::CREATED);
    assert_eq!(second_status, StatusCode::CREATED);
    assert_eq!(blank_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first["credential"]["id"], replay["credential"]["id"]);
    assert_eq!(
        first["credential"]["version"],
        replay["credential"]["version"]
    );
    assert_ne!(first["credential"]["id"], second["credential"]["id"]);
    assert_eq!(
        harness.credentials.list("workspace-a").await.unwrap().len(),
        2
    );
}

// Provider configuration ownership rule:
// C1 Vertex descriptor + project/location configuration causes E1 server-owned
// endpoint construction; the client neither supplies nor computes a base URL.
#[tokio::test]
async fn oauth_connection_uses_the_same_command_and_persists_only_the_helper() {
    let harness = harness();
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-oauth-command",
            "workspace_id":"workspace-a",
            "provider_id":"vertex",
            "display_name":"Vertex AI",
            "dialect":"vertex_gemini",
            "configuration":{"project_id":"p", "location":"global"},
            "oauth_helper":"gcloud"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["credential"]["kind"], "oauth");
    assert_eq!(result["credential"]["oauth_helper"], "gcloud");
    assert_eq!(
        result["endpoint"]["base_url"],
        "https://aiplatform.googleapis.com/v1/projects/p/locations/global/"
    );
    assert!(result.get("oauth_command").is_none());
    assert_eq!(
        harness.discovery.calls.lock().unwrap().as_slice(),
        &[(
            "vertex.vertex_gemini".into(),
            "cred:provider-connection-probe".into()
        )]
    );
    let sources = harness.credentials.list("workspace-a").await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider_id.as_deref(), Some("vertex"));
}

#[tokio::test]
async fn existing_credential_connection_reuses_the_source_without_creating_a_duplicate() {
    let harness = harness();
    let (status, entered) = call(
        &harness.app,
        "POST",
        "/v1/config/credentials",
        json!({
            "workspace_id":"workspace-a",
            "kind":"vault",
            "provider_id":"openai",
            "secret":"test-value" // awaken-allow: secret
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{entered}");
    let credential_id = entered["id"].as_str().unwrap();
    let (status, result) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-existing-openai-command",
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
            "dialect":"open_ai_responses",
            "credential_source_id":credential_id
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["credential"]["id"], credential_id);
    assert_eq!(
        harness.credentials.list("workspace-a").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn endpoint_scoped_connection_credential_cannot_be_reused_for_a_sibling_endpoint() {
    // Cause/effect decision table:
    // R1 connection-owned credential + the endpoint it proved -> accepted;
    // R2 the same credential + a sibling endpoint under the same Provider ->
    // rejected before discovery; R3 an ordinary provider-wide credential remains
    // reusable (covered by `existing_credential_connection_reuses...`).
    let harness = harness();
    let (primary_status, primary) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"primary-command",
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
            "dialect":"open_ai_chat",
            "endpoint_name":"primary",
            "secret":"endpoint-fixture" // awaken-allow: secret -- inert fixture
        }),
    )
    .await;
    assert_eq!(primary_status, StatusCode::CREATED, "R1: {primary}");
    let credential_id = primary["credential"]["id"].as_str().unwrap();
    let calls_before = harness.discovery.calls.lock().unwrap().len()
        + harness.discovery.secret_calls.lock().unwrap().len();

    let (sibling_status, sibling) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"sibling-command",
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
            "dialect":"open_ai_chat",
            "endpoint_name":"sibling",
            "credential_source_id":credential_id
        }),
    )
    .await;
    assert_eq!(
        sibling_status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "R2: {sibling}"
    );
    assert_eq!(
        harness.discovery.calls.lock().unwrap().len()
            + harness.discovery.secret_calls.lock().unwrap().len(),
        calls_before,
        "R2 fails before provider discovery"
    );
}

#[tokio::test]
async fn provider_connection_rejects_a_claude_code_setup_token_before_discovery() {
    let harness = harness();
    let (status, entered) = call(
        &harness.app,
        "POST",
        "/v1/config/credentials",
        json!({
            "workspace_id":"workspace-a",
            "kind":"vault",
            "provider_id":"anthropic",
            "env_key":"CLAUDE_CODE_OAUTH_TOKEN",
            "secret":"test-value" // awaken-allow: secret
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{entered}");
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-setup-token-command",
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "credential_source_id":entered["id"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "connection_auth_unsupported");
    assert!(harness.discovery.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn connection_rejects_parallel_or_unsupported_auth_inputs() {
    let harness = harness();
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-parallel-auth-command",
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "secret":"one",
            "oauth_helper":"gcloud"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "connection_auth_invalid");

    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-unsupported-auth-command",
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "oauth_helper":"gcloud"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "connection_auth_unsupported");
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
            "idempotency_key":"test-failed-discovery-command",
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
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
async fn empty_key_fails_before_discovery() {
    // Credential-validation rule: C1 the selected API-key value is empty.
    // E1 return 422; E2 do not invoke discovery; E3 persist no Provider fact.
    // Provider identity/template behavior is owned by the service-local
    // decision table, so this adapter case deliberately varies only C1.
    let harness = harness();
    let (status, _) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        json!({
            "idempotency_key":"test-empty-key-command",
            "workspace_id":"workspace-a",
            "provider_id":"openai",
            "display_name":"OpenAI",
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
            "idempotency_key":"test-catalog-rejection-command",
            "workspace_id":"workspace-a",
            "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages",
            "base_url":"https://provider.invalid/v1",
            "secret":"sealed-before-catalog-rejection" // awaken-allow: secret -- inert fixture
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
            "idempotency_key":"test-summary-command",
            "workspace_id":"workspace-a", "provider_id":"anthropic",
            "display_name":"Anthropic",
            "dialect":"anthropic_messages", "secret":"summary-secret" // awaken-allow: secret -- inert fixture
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
            &awaken_model_catalog::ProtocolEndpointId::new("anthropic.anthropic_messages"),
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
            &awaken_model_catalog::ProtocolEndpointId::new("anthropic.anthropic_messages"),
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
            "secret":"credential-only-secret" // awaken-allow: secret -- inert fixture
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

    harness
        .catalog
        .put_provider(Provider {
            id: ProviderId::new("gemini"),
            slug: "gemini".into(),
            display_name: "Gemini".into(),
            version: 1,
        })
        .await
        .unwrap();
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
    let request = connection_request("workspace-a", &credential_id);
    let (status, first) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let before = harness.catalog.snapshot().await.unwrap();

    *harness.discovery.fail.lock().unwrap() = true;
    *harness.discovery.models.lock().unwrap() = Vec::new();
    let (status, problem) = call(
        &harness.app,
        "POST",
        "/v1/config/provider-connections",
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
        "/v1/config/provider-connections",
        connection_request("workspace-b", &credential_id),
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
