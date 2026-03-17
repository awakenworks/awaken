#![allow(missing_docs)]

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use phoenix_test_helpers::{
    attr_str, ensure_phoenix_healthy, setup_otel_to_phoenix, start_single_response_server,
    start_sse_server, unique_suffix, wait_for_span_with_model, PhoenixConfig,
};
use serde_json::json;
use std::sync::Arc;
use tirea_agentos::composition::{
    AgentDefinition, AgentDefinitionSpec, AgentOsBuilder, ModelDefinition,
};
use tirea_agentos::contracts::storage::{MailboxStore, ThreadStore};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_extension_observability::{InMemorySink, LLMMetryPlugin};
use tower::ServiceExt;

mod common;

use common::compose_http_app;

fn test_mailbox_svc(os: &Arc<AgentOs>, store: Arc<dyn MailboxStore>) -> Arc<MailboxService> {
    Arc::new(MailboxService::new(os.clone(), store, "test"))
}

fn make_os(
    write_store: Arc<dyn ThreadStore>,
    provider_client: genai::Client,
    configured_model: &str,
    wire_model: &str,
    observed_model: &str,
    observed_provider: &str,
) -> AgentOs {
    let plugin = Arc::new(
        LLMMetryPlugin::new(InMemorySink::new())
            .with_model(observed_model)
            .with_provider(observed_provider),
    );

    let def = AgentDefinition {
        id: "test".to_string(),
        model: configured_model.to_string(),
        behavior_ids: vec!["llmmetry".into()],
        max_rounds: 1,
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior("llmmetry", plugin)
        .with_provider(observed_provider, provider_client)
        .with_model(
            configured_model,
            ModelDefinition::new(observed_provider, wire_model),
        )
        .with_agent_spec(AgentDefinitionSpec::local_with_id("test", def))
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs")
}

fn ai_sdk_messages_payload(thread_id: &str, input: &str, run_id: &str) -> serde_json::Value {
    json!({
        "id": thread_id,
        "messages": [
            { "role": "user", "content": input }
        ],
        "runId": run_id
    })
}

async fn require_phoenix(test_name: &str) -> Option<PhoenixConfig> {
    let cfg = PhoenixConfig::from_env();
    if !ensure_phoenix_healthy(&cfg.base_url).await {
        eprintln!(
            "Skipping {test_name}: Phoenix unavailable at {}",
            cfg.base_url
        );
        return None;
    }
    Some(cfg)
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_agentos_server_exports_llm_observability_to_phoenix() {
    let Some(cfg) =
        require_phoenix("e2e_agentos_server_exports_llm_observability_to_phoenix").await
    else {
        return;
    };

    let (base_url, _server) = start_sse_server(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello from mock llm\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ])
    .await
    .unwrap_or_else(|e| panic!("failed to start local mock SSE LLM server for Phoenix e2e: {e}"));

    let (_guard, tracer_provider) =
        setup_otel_to_phoenix(&cfg.otlp_traces_endpoint, "agentos-server-phoenix-e2e");

    let now_ms = unique_suffix();
    let observed_model = format!("phoenix-server-e2e-{now_ms}");
    let configured_model = "observed-model";
    let provider_name = "local_test_provider";

    let provider_client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = Arc::new(make_os(
        storage.clone(),
        provider_client,
        configured_model,
        "gpt-4",
        &observed_model,
        provider_name,
    ));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = ai_sdk_messages_payload(
        &format!("phoenix-server-session-{now_ms}"),
        "Say hello in one short sentence.",
        &format!("phoenix-server-run-{now_ms}"),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .expect("request to agentos-server route should succeed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let stream_payload = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        stream_payload.contains(r#""type":"finish""#),
        "ai-sdk stream should contain finish event"
    );

    let _ = tracer_provider.force_flush();

    let span = wait_for_span_with_model(&cfg.project_spans_url, &observed_model)
        .await
        .expect("expected a Phoenix span tagged with the observed model");
    let attrs = span
        .get("attributes")
        .and_then(serde_json::Value::as_object)
        .expect("span should include attributes");

    assert_eq!(
        attrs
            .get("gen_ai.request.model")
            .and_then(serde_json::Value::as_str),
        Some(observed_model.as_str())
    );
    assert_eq!(
        attrs
            .get("gen_ai.provider.name")
            .and_then(serde_json::Value::as_str),
        Some(provider_name)
    );
    assert!(
        span.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name.starts_with("chat")),
        "expected chat* span name"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_agentos_server_exports_llm_error_observability_to_phoenix() {
    let Some(cfg) =
        require_phoenix("e2e_agentos_server_exports_llm_error_observability_to_phoenix").await
    else {
        return;
    };

    let (base_url, _server) = start_single_response_server(
        "500 Internal Server Error",
        "application/json",
        r#"{"error":"fail"}"#,
    )
    .await
    .unwrap_or_else(|e| panic!("failed to start local mock HTTP LLM server for Phoenix e2e: {e}"));

    let (_guard, tracer_provider) =
        setup_otel_to_phoenix(&cfg.otlp_traces_endpoint, "agentos-server-phoenix-e2e");

    let now_ms = unique_suffix();
    let observed_model = format!("phoenix-server-e2e-err-{now_ms}");
    let configured_model = "observed-model";
    let provider_name = "local_test_provider";

    let provider_client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = Arc::new(make_os(
        storage.clone(),
        provider_client,
        configured_model,
        "gpt-4",
        &observed_model,
        provider_name,
    ));
    let mailbox_svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mailbox_svc));

    let payload = ai_sdk_messages_payload(
        &format!("phoenix-server-session-err-{now_ms}"),
        "Say hello in one short sentence.",
        &format!("phoenix-server-run-err-{now_ms}"),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .expect("request to agentos-server route should succeed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let stream_payload = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        stream_payload.contains(r#""type":"error""#),
        "ai-sdk stream should contain error event when upstream LLM fails"
    );

    let _ = tracer_provider.force_flush();

    let span = wait_for_span_with_model(&cfg.project_spans_url, &observed_model)
        .await
        .expect("expected a Phoenix error span tagged with the observed model");
    assert_eq!(
        attr_str(&span, "error.type"),
        Some("llm_stream_event_error"),
        "expected llm_stream_event_error on exported chat span"
    );
}
