//! A2A HTTP integration tests for the current A2A v1.0 surface.

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

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
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
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

async fn request_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }

    let req = req
        .body(match body {
            Some(body) => axum::body::Body::from(body.to_string()),
            None => axum::body::Body::empty(),
        })
        .expect("request build");

    let resp = app
        .clone()
        .oneshot(req)
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    let body = String::from_utf8(body.to_vec()).expect("utf-8");
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).expect("valid json")
    };

    (status, json)
}

fn send_message_payload(task_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "message": {
            "taskId": task_id,
            "contextId": task_id,
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [{"text": text}]
        }
    })
}

#[tokio::test]
async fn well_known_agent_card_returns_latest_shape() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(&app, "GET", "/.well-known/agent-card.json", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str(), Some("alpha"));
    assert_eq!(
        body["supportedInterfaces"][0]["url"].as_str(),
        Some("http://localhost/v1/a2a")
    );
    assert_eq!(
        body["supportedInterfaces"][0]["protocolBinding"].as_str(),
        Some("HTTP+JSON")
    );
    assert_eq!(
        body["supportedInterfaces"][0]["protocolVersion"].as_str(),
        Some("1.0")
    );
    assert_eq!(body["provider"]["organization"].as_str(), Some("Awaken"));
    assert_eq!(body["provider"]["url"].as_str(), Some("http://localhost"));
    assert_eq!(body["capabilities"]["streaming"].as_bool(), Some(false));
    assert_eq!(
        body["capabilities"]["pushNotifications"].as_bool(),
        Some(false)
    );
    assert_eq!(
        body["capabilities"]["extendedAgentCard"].as_bool(),
        Some(false)
    );
    assert!(
        body.get("url").is_none(),
        "legacy top-level url must not be present"
    );
}

#[tokio::test]
async fn message_send_returns_task_wrapper_and_task_is_retrievable() {
    let app = make_test_app(&["alpha"]);
    let task_id = "task-latest-a2a";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(task_id, "msg-1", "hello")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(task_id));
    assert_eq!(
        body["task"]["status"]["state"].as_str(),
        Some("TASK_STATE_COMPLETED")
    );

    let (status, task) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}?historyLength=10"),
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["id"].as_str(), Some(task_id));
    let history = task["history"].as_array().expect("history array");
    assert!(
        history.iter().any(|message| {
            message["messageId"].as_str() == Some("msg-1")
                && message["role"].as_str() == Some("ROLE_USER")
        }),
        "user message missing from history: {task}"
    );
}

#[tokio::test]
async fn tenant_message_send_is_visible_in_tenant_task_list() {
    let app = make_test_app(&["alpha"]);
    let task_id = "tenant-task-1";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            "msg-tenant-1",
            "hello tenant",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));

    let (status, body) = request_json(&app, "GET", "/v1/a2a/alpha/tasks", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tasks"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tasks"][0]["id"].as_str(), Some(task_id));
    assert_eq!(body["tasks"][0]["contextId"].as_str(), Some(task_id));
}

#[tokio::test]
async fn message_stream_is_unimplemented_when_streaming_not_advertised() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:stream",
        &[("content-type", "application/json")],
        Some(send_message_payload("task-stream", "msg-stream", "hello")),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        body["error"]["details"][0]["reason"].as_str(),
        Some("UNSUPPORTED_OPERATION")
    );
}

#[tokio::test]
async fn unsupported_version_returns_failed_precondition_error() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(
        &app,
        "GET",
        "/.well-known/agent-card.json",
        &[("a2a-version", "0.9")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"]["details"][0]["reason"].as_str(),
        Some("VERSION_NOT_SUPPORTED")
    );
    assert_eq!(
        body["error"]["details"][0]["metadata"]["requestedVersion"].as_str(),
        Some("0.9")
    );
}

#[tokio::test]
async fn invalid_inbound_message_role_is_rejected() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-invalid-role",
                "contextId": "task-invalid-role",
                "messageId": "msg-invalid-role",
                "role": "ROLE_AGENT",
                "parts": [{"text": "hello"}]
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
    assert_eq!(
        body["error"]["details"][0]["fieldViolations"][0]["field"].as_str(),
        Some("message.role")
    );
}
