use super::*;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap as AxumHeaderMap, StatusCode};
use axum::response::Json;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;

#[test]
fn model_list_url_appends_models_to_base_path() {
    let provider = ProviderSpec {
        adapter: "openai".into(),
        base_url: Some("https://example.test/v1".into()),
        ..ProviderSpec::default()
    };

    assert_eq!(
        model_list_url(&provider).unwrap().as_str(),
        "https://example.test/v1/models"
    );
}

#[test]
fn model_list_url_rejects_inference_endpoint_base_url() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "openai".into(),
        base_url: Some("https://example.test/v1/chat/completions".into()),
        ..ProviderSpec::default()
    };

    assert!(model_list_url(&provider).is_none());
}

#[test]
fn default_openai_discovery_requires_explicit_credentials() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "openai".into(),
        ..ProviderSpec::default()
    };

    assert!(should_skip_unauthenticated_default_endpoint(&provider));
}

#[test]
fn vertex_discovery_has_no_implicit_endpoint() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "vertex".into(),
        ..ProviderSpec::default()
    };

    assert!(model_list_url(&provider).is_none());
}

#[test]
fn referenced_models_include_pool_members_once() {
    let models = vec![
        ModelSpec::new("m0", "p", "gpt-4o"),
        ModelSpec {
            context_window: Some(10),
            max_output_tokens: Some(10),
            modalities: awaken_server_contract::registry_spec::Modalities {
                input: vec![awaken_server_contract::registry_spec::Modality::Text],
                output: vec![awaken_server_contract::registry_spec::Modality::Text],
            },
            knowledge_cutoff: Some("2025-01".into()),
            ..ModelSpec::new("m1", "p", "complete")
        },
    ];
    let pools = vec![ModelPoolSpec {
        id: "pool".into(),
        members: vec![awaken_server_contract::registry_spec::PoolMemberSpec {
            model_id: "m0".into(),
            role: awaken_server_contract::registry_spec::PoolMemberRole::Member,
            weight: None,
        }],
        routing: Default::default(),
        switch: Default::default(),
    }];
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "openai".into(),
        ..ProviderSpec::default()
    }];

    let wanted = referenced_models_by_provider(&providers, &models, &pools);

    assert_eq!(wanted["p"].len(), 1);
    assert!(wanted["p"].contains("gpt-4o"));
}

#[test]
fn gemini_model_missing_only_cutoff_is_not_requested() {
    // Gemini discovery cannot fill modalities or knowledge cutoff, so a
    // model that already has token limits must not keep re-triggering a
    // probe just because those fields are absent.
    let providers = vec![ProviderSpec {
        id: "g".into(),
        adapter: "gemini".into(),
        ..ProviderSpec::default()
    }];
    let models = vec![ModelSpec {
        context_window: Some(1_000_000),
        max_output_tokens: Some(8_192),
        ..ModelSpec::new("m", "g", "gemini-2.5-pro")
    }];

    let wanted = referenced_models_by_provider(&providers, &models, &[]);

    assert!(
        wanted.is_empty(),
        "token limits present and Gemini cannot fill modalities/cutoff: no probe"
    );
}

#[test]
fn gemini_model_missing_token_limits_is_requested() {
    // Token limits are discoverable on Gemini, so a missing one still drives
    // a probe.
    let providers = vec![ProviderSpec {
        id: "g".into(),
        adapter: "gemini".into(),
        ..ProviderSpec::default()
    }];
    let models = vec![ModelSpec::new("m", "g", "gemini-2.5-pro")];

    let wanted = referenced_models_by_provider(&providers, &models, &[]);

    assert!(wanted.contains_key("g"));
}

#[test]
fn discovery_auth_defaults_to_schema_not_adapter() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        adapter_options: [("model_discovery_schema".to_string(), json!("gemini"))]
            .into_iter()
            .collect(),
        ..ProviderSpec::default()
    };
    let schema = provider_discovery_schema(&provider).expect("schema");
    let headers = auth_headers(&provider, schema)
        .expect("valid auth")
        .expect("headers");

    assert_eq!(
        headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("secret")
    );
    assert!(!headers.contains_key(AUTHORIZATION));
}

#[test]
fn discovery_auth_override_can_select_bearer_independently() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        adapter_options: [
            ("model_discovery_schema".to_string(), json!("gemini")),
            ("model_discovery_auth".to_string(), json!("bearer")),
        ]
        .into_iter()
        .collect(),
        ..ProviderSpec::default()
    };
    let schema = provider_discovery_schema(&provider).expect("schema");
    let headers = auth_headers(&provider, schema)
        .expect("valid auth")
        .expect("headers");

    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret")
    );
    assert!(!headers.contains_key("x-goog-api-key"));
}

#[test]
fn discovery_headers_merge_custom_headers_without_auth_override() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "openrouter".into(),
        api_key: Some("secret".into()),
        adapter_options: [(
            "headers".to_string(),
            json!({
                "X-Tenant-Id": "team-42",
                "Authorization": "Bearer wrong",
                "x-goog-api-key": "wrong",
                "Proxy-Authorization": "Basic wrong",
                "Cookie": "sid=wrong",
                "X-API-Key": "wrong",
                "api-key": "wrong",
                "Ocp-Apim-Subscription-Key": "wrong",
                "X-Auth-Token": "wrong"
            }),
        )]
        .into_iter()
        .collect(),
        ..ProviderSpec::default()
    };
    let headers = discovery_headers(&provider, "openai")
        .expect("valid headers")
        .expect("headers");

    assert_eq!(
        headers
            .get("x-tenant-id")
            .and_then(|value| value.to_str().ok()),
        Some("team-42")
    );
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret")
    );
    assert!(!headers.contains_key("x-goog-api-key"));
    assert!(!headers.contains_key("proxy-authorization"));
    assert!(!headers.contains_key("cookie"));
    assert!(!headers.contains_key("x-api-key"));
    assert!(!headers.contains_key("api-key"));
    assert!(!headers.contains_key("ocp-apim-subscription-key"));
    assert!(!headers.contains_key("x-auth-token"));
}

#[test]
fn discovery_auth_none_strips_auth_like_custom_headers() {
    let provider = ProviderSpec {
        id: "p".into(),
        adapter: "openrouter".into(),
        api_key: Some("secret".into()),
        adapter_options: [
            ("model_discovery_auth".to_string(), json!("none")),
            (
                "headers".to_string(),
                json!({
                    "X-Tenant-Id": "team-42",
                    "Authorization": "Bearer wrong",
                    "X-API-Key": "wrong",
                    "Cookie": "sid=wrong"
                }),
            ),
        ]
        .into_iter()
        .collect(),
        ..ProviderSpec::default()
    };
    let headers = discovery_headers(&provider, "openai")
        .expect("valid headers")
        .expect("tenant header remains");

    assert_eq!(
        headers
            .get("x-tenant-id")
            .and_then(|value| value.to_str().ok()),
        Some("team-42")
    );
    assert!(!headers.contains_key(AUTHORIZATION));
    assert!(!headers.contains_key("x-api-key"));
    assert!(!headers.contains_key("cookie"));
}

#[tokio::test]
async fn discovers_openai_compatible_capabilities_from_models_endpoint() {
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "openrouter".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: Default::default(),
    }];
    let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    let patch = result.discovered["p"].get("openai/gpt-4o").expect("patch");
    assert_eq!(patch.context_window, Some(128_000));
    assert_eq!(patch.max_output_tokens, Some(16_384));
    assert!(result.attempted.contains("p"), "p was probed");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_discovery_auth_is_not_attempted() {
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: [
            (
                "model_discovery_schema".to_string(),
                json!("openai-compatible"),
            ),
            ("model_discovery_auth".to_string(), json!("no_auth")),
        ]
        .into_iter()
        .collect(),
    }];
    let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert!(result.discovered.is_empty());
    assert!(
        result.attempted.is_empty(),
        "invalid auth config must fail closed before issuing discovery"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "invalid auth config must not send credentials or issue request"
    );
}

#[tokio::test]
async fn successful_discovery_without_wanted_models_returns_full_snapshot() {
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "openrouter".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: Default::default(),
    }];
    let models = vec![ModelSpec::new("m", "p", "missing-model")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.discovered.get("p"),
        Some(&HashMap::from([(
            "openai/gpt-4o".to_string(),
            ModelCapabilityPatch {
                context_window: Some(128_000),
                max_output_tokens: Some(16_384),
                modalities: None,
                knowledge_cutoff: None,
            }
        )]))
    );
}

#[tokio::test]
async fn fully_specified_models_are_not_attempted() {
    // Every model capability is explicit, so no provider needs discovery:
    // nothing is attempted and the stale-snapshot warning cannot fire.
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "openrouter".into(),
        api_key: Some("secret".into()),
        base_url: Some("https://example.test/v1".into()),
        timeout_secs: 5,
        adapter_options: Default::default(),
    }];
    let models = vec![ModelSpec {
        context_window: Some(128_000),
        max_output_tokens: Some(16_384),
        modalities: awaken_server_contract::registry_spec::Modalities {
            input: vec![awaken_server_contract::registry_spec::Modality::Text],
            output: vec![awaken_server_contract::registry_spec::Modality::Text],
        },
        knowledge_cutoff: Some("2025-01".into()),
        ..ModelSpec::new("m", "p", "gpt-4o")
    }];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert!(result.discovered.is_empty());
    assert!(
        result.attempted.is_empty(),
        "no discovery was needed, so no provider is attempted"
    );
}

#[tokio::test]
async fn unknown_adapter_is_not_probed_without_opt_in() {
    // An unknown adapter with an explicit base_url must NOT be probed and
    // parsed as trusted OpenAI metadata.
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: Default::default(),
    }];
    let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert!(result.discovered.is_empty());
    assert!(result.attempted.is_empty());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "unknown adapter must not be probed"
    );
}

#[tokio::test]
async fn custom_adapter_is_probed_with_explicit_schema_opt_in() {
    // A custom OpenAI-compatible gateway opts in via adapter_options.
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: [(
            "model_discovery_schema".to_string(),
            json!("openai-compatible"),
        )]
        .into_iter()
        .collect(),
    }];
    let models = vec![ModelSpec::new("m", "p", "openai/gpt-4o")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "an opted-in custom adapter is probed"
    );
    let patch = result.discovered["p"].get("openai/gpt-4o").expect("patch");
    assert_eq!(patch.context_window, Some(128_000));
    assert!(result.attempted.contains("p"));
}

#[tokio::test]
async fn custom_adapter_with_gemini_schema_uses_google_api_key_auth() {
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_gemini_models_server(Arc::clone(&hits)).await;
    let providers = vec![ProviderSpec {
        id: "p".into(),
        adapter: "custom-gateway".into(),
        api_key: Some("secret".into()),
        base_url: Some(base_url),
        timeout_secs: 5,
        adapter_options: [("model_discovery_schema".to_string(), json!("gemini"))]
            .into_iter()
            .collect(),
    }];
    let models = vec![ModelSpec::new("m", "p", "gemini-2.5-pro")];

    let result = discover_provider_capabilities(&providers, &models, &[]).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "custom Gemini-schema discovery must use x-goog-api-key"
    );
    let patch = result.discovered["p"].get("gemini-2.5-pro").expect("patch");
    assert_eq!(patch.context_window, Some(1_048_576));
    assert_eq!(patch.max_output_tokens, Some(65_536));
    assert!(result.attempted.contains("p"));
}

async fn spawn_gemini_models_server(hits: Arc<AtomicUsize>) -> String {
    async fn handler(
        State(hits): State<Arc<AtomicUsize>>,
        headers: AxumHeaderMap,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        let Some(api_key) = headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok())
        else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if api_key != "secret" {
            return Err(StatusCode::UNAUTHORIZED);
        }
        hits.fetch_add(1, Ordering::SeqCst);
        Ok(Json(json!({
            "models": [{
                "name": "models/gemini-2.5-pro",
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536
            }]
        })))
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    let app = Router::new()
        .route("/v1beta/models", get(handler))
        .with_state(hits);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1beta")
}

async fn spawn_models_server(hits: Arc<AtomicUsize>) -> String {
    async fn handler(
        State(hits): State<Arc<AtomicUsize>>,
        headers: AxumHeaderMap,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        let Some(auth) = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if auth != "Bearer secret" {
            return Err(StatusCode::UNAUTHORIZED);
        }
        hits.fetch_add(1, Ordering::SeqCst);
        Ok(Json(json!({
            "data": [{
                "id": "openai/gpt-4o",
                "context_length": 128000,
                "top_provider": { "max_completion_tokens": 16384 }
            }]
        })))
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    let app = Router::new()
        .route("/v1/models", get(handler))
        .with_state(hits);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}
