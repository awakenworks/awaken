use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadStore};
use tirea_agentos::orchestrator::{AgentDefinition, AgentOs, AgentOsBuilder};
use tirea_agentos_server::service::AppState;
use tirea_agentos_server::{http, protocol};
use tirea_store_adapters::MemoryStore;
use tower::ServiceExt;

mod common;

use common::TerminatePlugin;

fn make_os(write_store: Arc<dyn ThreadStore>) -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_explicit".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_explicit",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_explicit")),
        )
        .with_agent("test", def)
        .with_agent_state_store(write_store)
        .build()
        .unwrap()
}

fn explicit_http_app(os: Arc<AgentOs>, read_store: Arc<MemoryStore>) -> axum::Router {
    axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .with_state(AppState { os, read_store })
}

#[tokio::test]
async fn explicit_composition_serves_agui_and_ai_sdk_with_shared_runtime() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = explicit_http_app(os, store.clone());

    let ai_req = Request::builder()
        .method("POST")
        .uri("/v1/ai-sdk/agents/test/runs")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "id": "thread-explicit-ai-sdk",
                "messages": [
                    { "id": "m1", "role": "user", "content": "hello ai-sdk" }
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let ai_resp = app.clone().oneshot(ai_req).await.unwrap();
    assert_eq!(ai_resp.status(), StatusCode::OK);
    assert_eq!(
        ai_resp
            .headers()
            .get("x-tirea-ai-sdk-version")
            .and_then(|v| v.to_str().ok()),
        Some("v6")
    );
    let ai_body = to_bytes(ai_resp.into_body(), usize::MAX).await.unwrap();
    let ai_text = String::from_utf8_lossy(&ai_body);
    assert!(ai_text.contains("\"type\":\"start\""), "{ai_text}");
    assert!(ai_text.contains("\"type\":\"finish\""), "{ai_text}");

    let ag_req = Request::builder()
        .method("POST")
        .uri("/v1/ag-ui/agents/test/runs")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "threadId": "thread-explicit-agui",
                "runId": "run-explicit-agui",
                "messages": [
                    { "id": "u1", "role": "user", "content": "hello agui" }
                ],
                "tools": []
            })
            .to_string(),
        ))
        .unwrap();

    let ag_resp = app.clone().oneshot(ag_req).await.unwrap();
    assert_eq!(ag_resp.status(), StatusCode::OK);
    let ag_body = to_bytes(ag_resp.into_body(), usize::MAX).await.unwrap();
    let ag_text = String::from_utf8_lossy(&ag_body);
    assert!(ag_text.contains("RUN_STARTED"), "{ag_text}");
    assert!(ag_text.contains("RUN_FINISHED"), "{ag_text}");

    let ai_thread = store
        .load_thread("thread-explicit-ai-sdk")
        .await
        .expect("load ai-sdk thread");
    assert!(ai_thread.is_some());

    let ag_thread = store
        .load_thread("thread-explicit-agui")
        .await
        .expect("load agui thread");
    assert!(ag_thread.is_some());
}
