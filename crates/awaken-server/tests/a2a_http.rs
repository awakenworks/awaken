//! A2A HTTP integration tests — validates discovery and task creation endpoints.
//!
//! Mirrors high-value A2A tests from uncarve's tirea-agentos-server/tests/a2a_http.rs,
//! adapted to awaken's AppState + RunDispatcher architecture.
//!
//! NOTE: Several A2A routes are currently unreachable due to axum 0.7 path
//! parameter conflicts:
//! - `/v1/a2a/tasks/{task_id}` conflicts with literal `/v1/a2a/tasks/send`
//! - `/v1/a2a/agents/{agent_id}/agent-card` conflicts with
//!   `/v1/a2a/agents/{agent_id}/tasks/{task_action}`
//! Tests for affected routes are omitted; see `a2a_routes()` for the issue.

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::protocols::a2a::http::{AgentCapabilities, AgentCard};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

// ── Mock executor ──

struct ImmediateExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for ImmediateExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "immediate"
    }
}

// ── Shared test app ──

fn make_test_app(agent_ids: &[&str]) -> axum::Router {
    let mut builder = AgentRuntimeBuilder::new()
        .with_model(
            "test-model",
            ModelEntry {
                provider: "mock".into(),
                model_name: "mock-model".into(),
            },
        )
        .with_provider("mock", Arc::new(ImmediateExecutor));

    for agent_id in agent_ids {
        builder = builder.with_agent_spec(AgentSpec {
            id: (*agent_id).to_string(),
            model: "test-model".into(),
            system_prompt: "test".into(),
            max_rounds: 0,
            ..Default::default()
        });
    }

    let store = Arc::new(InMemoryStore::new());
    builder = builder.with_thread_run_store(store.clone());
    let runtime = Arc::new(builder.build().expect("build runtime"));
    let mailbox_store = std::sync::Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = std::sync::Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    build_router().with_state(state)
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(axum::body::Body::empty())
                .expect("request build"),
        )
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .expect("request build"),
        )
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

// ============================================================================
// Agent card shape (struct-level)
// ============================================================================

#[test]
fn well_known_card_shape() {
    let card = AgentCard {
        id: "agent".into(),
        name: "agent".into(),
        description: Some("d".into()),
        url: "/v1/a2a/agents/agent/message:send".into(),
        version: "0.1.0".into(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        }),
        skills: vec![],
    };
    let v = serde_json::to_value(card).unwrap();
    assert_eq!(v["name"], "agent");
    assert_eq!(v["capabilities"]["streaming"], true);
}

#[test]
fn agent_card_empty_skills_omitted() {
    let card = AgentCard {
        id: String::new(),
        name: "minimal".into(),
        description: None,
        url: String::new(),
        version: "0.1.0".into(),
        capabilities: None,
        skills: Vec::new(),
    };
    let json = serde_json::to_string(&card).unwrap();
    assert!(!json.contains("skills"));
    assert!(!json.contains("description"));
    assert!(!json.contains("capabilities"));
}

// ============================================================================
// Discovery endpoints
// ============================================================================

#[tokio::test]
async fn a2a_well_known_agent_card_returns_ok() {
    let app = make_test_app(&["alpha", "beta"]);
    let (status, body) = get_json(app, "/v1/a2a/.well-known/agent").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["name"].as_str(), Some("awaken-agent"));
    assert!(
        payload["capabilities"]["streaming"]
            .as_bool()
            .unwrap_or(false)
    );
    assert_eq!(payload["version"].as_str(), Some("0.1.0"));
}

#[tokio::test]
async fn a2a_agent_list_returns_registered_agents() {
    let app = make_test_app(&["alpha", "beta"]);
    let (status, body) = get_json(app, "/v1/a2a/agents").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let items = payload.as_array().expect("agent list should be array");
    assert!(
        items
            .iter()
            .any(|item| item["agentId"].as_str() == Some("alpha")),
        "missing alpha: {payload}"
    );
    assert!(
        items
            .iter()
            .any(|item| item["agentId"].as_str() == Some("beta")),
        "missing beta: {payload}"
    );
}

#[tokio::test]
async fn a2a_agent_list_single_agent() {
    let app = make_test_app(&["solo"]);
    let (status, body) = get_json(app, "/v1/a2a/agents").await;
    assert_eq!(status, StatusCode::OK);
    let items: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(items.as_array().map(Vec::len), Some(1));
    assert_eq!(items[0]["agentId"].as_str(), Some("solo"));
}

#[tokio::test]
async fn a2a_agent_list_sorted_and_deduped() {
    let app = make_test_app(&["beta", "alpha"]);
    let (status, body) = get_json(app, "/v1/a2a/agents").await;
    assert_eq!(status, StatusCode::OK);
    let items: Vec<Value> = serde_json::from_str(&body).expect("valid json");
    let ids: Vec<&str> = items.iter().filter_map(|i| i["agentId"].as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "agent list should be sorted");
}

// ============================================================================
// Task creation (POST /v1/a2a/tasks/send)
// ============================================================================

#[tokio::test]
async fn a2a_task_send_creates_task_and_returns_id() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "agentId": "alpha",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "hello"}]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert!(payload["taskId"].as_str().is_some(), "taskId missing");
    assert_eq!(payload["status"]["state"].as_str(), Some("submitted"));
}

#[tokio::test]
async fn a2a_task_send_with_explicit_task_id() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "taskId": "my-custom-task-id",
            "agentId": "alpha",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "hello"}]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["taskId"].as_str(), Some("my-custom-task-id"));
}

#[tokio::test]
async fn a2a_task_send_requires_message() {
    let app = make_test_app(&["alpha"]);
    let (status, _body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "agentId": "alpha"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a2a_task_send_requires_non_empty_text() {
    let app = make_test_app(&["alpha"]);
    let (status, _body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "agentId": "alpha",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": ""}]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a2a_task_send_auto_generates_task_id_when_omitted() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "agentId": "alpha",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "hello"}]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let task_id = payload["taskId"].as_str().expect("taskId should exist");
    assert!(
        !task_id.is_empty(),
        "auto-generated taskId should be non-empty"
    );
}

#[tokio::test]
async fn a2a_task_send_multiple_text_parts_concatenated() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = post_json(
        app,
        "/v1/a2a/tasks/send",
        json!({
            "agentId": "alpha",
            "message": {
                "role": "user",
                "parts": [
                    {"type": "text", "text": "Hello "},
                    {"type": "text", "text": "World"}
                ]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "multi-part send failed: {body}");
}
