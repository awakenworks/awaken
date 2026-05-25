#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
//! A2A HTTP integration tests for the current A2A v1.0 surface.

use async_trait::async_trait;
use awaken_contract::ModelSpec;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::storage::{
    RunRecord, RunStore, RunWaitingState, ThreadRunStore, ThreadStore, WaitingReason,
};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::thread::Thread;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext,
    TaskResult as BackgroundTaskResult, TaskStatus,
};
use awaken_server::app::{ServerConfig, ServerState};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

struct DelayedExecutor;

#[async_trait]
impl LlmExecutor for DelayedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "delayed"
    }
}

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: calls,
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().expect("responses lock");
        Ok(if responses.is_empty() {
            StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            }
        } else {
            responses.remove(0)
        })
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct SpawnBackgroundChildTool {
    manager: Arc<BackgroundTaskManager>,
}

#[async_trait]
impl Tool for SpawnBackgroundChildTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_bg_child",
            "spawn_bg_child",
            "Spawn a cancellable background child task",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("leaf");
        let task_id = self
            .manager
            .spawn(
                &ctx.run_identity.thread_id,
                "background_child",
                Some(name),
                "child spawned from A2A test",
                TaskParentContext::default(),
                |task_ctx| async move {
                    task_ctx.cancelled().await;
                    BackgroundTaskResult::Cancelled
                },
            )
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        Ok(ToolResult::success("spawn_bg_child", json!({"task_id": task_id})).into())
    }
}

fn build_test_fixture<E>(
    agent_ids: &[&str],
    executor: Arc<E>,
    config: ServerConfig,
) -> (axum::Router, Arc<InMemoryStore>)
where
    E: LlmExecutor + 'static,
{
    let mut builder = AgentRuntimeBuilder::new()
        .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
        .with_provider("mock", executor);

    for agent_id in agent_ids {
        builder = builder.with_agent_spec(AgentSpec {
            id: (*agent_id).to_string(),
            model_id: "test-model".into(),
            system_prompt: "test".into(),
            max_rounds: 0,
            ..Default::default()
        });
    }

    let store = Arc::new(InMemoryStore::new());
    builder = builder.with_in_memory_thread_run_store(store.clone());
    let runtime = Arc::new(builder.build().expect("build runtime"));
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        config,
    );
    (build_router(&state), store)
}

fn build_test_app<E>(agent_ids: &[&str], executor: Arc<E>, config: ServerConfig) -> axum::Router
where
    E: LlmExecutor + 'static,
{
    build_test_fixture(agent_ids, executor, config).0
}

fn make_test_app(agent_ids: &[&str]) -> axum::Router {
    build_test_app(
        agent_ids,
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    )
}

fn build_background_cancel_fixture()
-> (axum::Router, Arc<InMemoryStore>, Arc<BackgroundTaskManager>) {
    let manager = Arc::new(BackgroundTaskManager::new());
    let executor = Arc::new(ScriptedLlm::new(vec![ScriptedLlm::tool_call_response(
        vec![
            ToolCall::new("call-spawn", "spawn_bg_child", json!({"name": "leaf"})),
            ToolCall::new(
                "call-cancel",
                "cancel_task",
                json!({"target": {"relation": "self"}}),
            ),
        ],
    )]));

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
            .with_provider("mock", executor)
            .with_agent_spec(AgentSpec {
                id: "alpha".into(),
                model_id: "test-model".into(),
                system_prompt: "cancel yourself after spawning a child".into(),
                max_rounds: 2,
                plugin_ids: vec!["background".into()],
                ..Default::default()
            })
            .with_tool(
                "spawn_bg_child",
                Arc::new(SpawnBackgroundChildTool {
                    manager: manager.clone(),
                }),
            )
            .with_plugin(
                "background",
                Arc::new(BackgroundTaskPlugin::new(manager.clone())),
            )
            .with_in_memory_thread_run_store(store.clone())
            .build()
            .expect("build runtime"),
    );
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    (build_router(&state), store, manager)
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

async fn request_text(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, String, String) {
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
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).expect("utf-8"),
    )
}

fn send_message_payload(task_id: &str, context_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "message": {
            "taskId": task_id,
            "contextId": context_id,
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [{"text": text}]
        }
    })
}

fn user_message_ids(task: &Value) -> Vec<&str> {
    task["history"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"].as_str() == Some("ROLE_USER"))
        .filter_map(|message| message["messageId"].as_str())
        .collect()
}

#[tokio::test]
async fn a2a_http_dispatch_matrix_handles_default_and_tenant_routes() {
    let app = make_test_app(&["alpha"]);

    let route_cases = [
        (
            "/v1/a2a/message:send",
            "/v1/a2a/tasks",
            "task-matrix-default",
            "thread-matrix-default",
        ),
        (
            "/v1/a2a/alpha/message:send",
            "/v1/a2a/alpha/tasks",
            "task-matrix-tenant",
            "thread-matrix-tenant",
        ),
    ];

    for (send_uri, tasks_uri, task_id, context_id) in route_cases {
        let (status, body) = request_json(
            &app,
            "POST",
            send_uri,
            &[("content-type", "application/json")],
            Some(send_message_payload(
                task_id,
                context_id,
                &format!("msg-{task_id}"),
                "hello",
            )),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "unexpected body for {send_uri}: {body}"
        );
        assert_eq!(body["task"]["id"].as_str(), Some(task_id));

        let (status, task) = request_json(
            &app,
            "GET",
            &format!("{tasks_uri}/{task_id}?historyLength=10"),
            &[],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "GET task failed: {task}");
        assert_eq!(task["id"].as_str(), Some(task_id));

        let (status, tasks) = request_json(&app, "GET", tasks_uri, &[], None).await;
        assert_eq!(status, StatusCode::OK, "list tasks failed: {tasks}");
        assert!(
            tasks["tasks"]
                .as_array()
                .expect("tasks array")
                .iter()
                .any(|task| task["id"].as_str() == Some(task_id)),
            "task {task_id} missing from {tasks_uri}: {tasks}"
        );

        let config_id = format!("cfg-{task_id}");
        let push_uri = format!("{tasks_uri}/{task_id}/pushNotificationConfigs");
        let (status, cfg) = request_json(
            &app,
            "POST",
            &push_uri,
            &[("content-type", "application/json")],
            Some(json!({
                "id": config_id.clone(),
                "url": "http://127.0.0.1:9/a2a-test"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create push config failed: {cfg}");
        assert_eq!(cfg["id"].as_str(), Some(config_id.as_str()));

        let (status, list) = request_json(&app, "GET", &push_uri, &[], None).await;
        assert_eq!(status, StatusCode::OK, "list push configs failed: {list}");
        assert_eq!(list["configs"][0]["id"].as_str(), Some(config_id.as_str()));

        let item_uri = format!("{push_uri}/{config_id}");
        let (status, cfg) = request_json(&app, "GET", &item_uri, &[], None).await;
        assert_eq!(status, StatusCode::OK, "get push config failed: {cfg}");
        assert_eq!(cfg["taskId"].as_str(), Some(task_id));

        let (status, _content_type, deleted) =
            request_text(&app, "DELETE", &item_uri, &[], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(deleted.is_empty());
    }

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/message:stream",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-matrix-stream",
            "thread-matrix-stream",
            "msg-matrix-stream",
            "stream",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("\"task\""),
        "missing stream task body: {body}"
    );
}

#[tokio::test]
async fn a2a_http_dispatch_matrix_handles_task_actions() {
    let app = build_test_app(
        &["alpha"],
        Arc::new(DelayedExecutor),
        ServerConfig::default(),
    );

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-matrix-cancel",
                "contextId": "thread-matrix-actions",
                "messageId": "msg-matrix-cancel",
                "role": "ROLE_USER",
                "parts": [{"text": "cancel me"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected cancel seed body: {body}"
    );

    let (status, canceled) = request_json(
        &app,
        "POST",
        "/v1/a2a/tasks/task-matrix-cancel:cancel",
        &[],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "cancel route returned unexpected body: {canceled}"
    );
    assert_eq!(
        canceled["error"]["details"][0]["reason"].as_str(),
        Some("TASK_NOT_CANCELABLE")
    );

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-matrix-subscribe",
                "contextId": "thread-matrix-actions",
                "messageId": "msg-matrix-subscribe",
                "role": "ROLE_USER",
                "parts": [{"text": "subscribe me"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected subscribe seed body: {body}"
    );

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/alpha/tasks/task-matrix-subscribe:subscribe",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("\"task\""),
        "missing subscribe task body: {body}"
    );

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-matrix-default-subscribe",
                "contextId": "thread-matrix-actions",
                "messageId": "msg-matrix-default-subscribe",
                "role": "ROLE_USER",
                "parts": [{"text": "subscribe default"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected default subscribe seed body: {body}"
    );

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/tasks/task-matrix-default-subscribe:subscribe",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("\"task\""),
        "missing default subscribe task body: {body}"
    );

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-matrix-tenant-cancel",
                "contextId": "thread-matrix-actions",
                "messageId": "msg-matrix-tenant-cancel",
                "role": "ROLE_USER",
                "parts": [{"text": "cancel tenant"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected tenant cancel seed body: {body}"
    );

    let (status, canceled) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/tasks/task-matrix-tenant-cancel:cancel",
        &[],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "tenant cancel route returned unexpected body: {canceled}"
    );
    assert_eq!(
        canceled["error"]["details"][0]["reason"].as_str(),
        Some("TASK_NOT_CANCELABLE")
    );
}

#[tokio::test]
async fn a2a_http_dispatch_matrix_handles_tenant_stream_route() {
    let app = make_test_app(&["alpha"]);

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/alpha/message:stream",
        &[("content-type", "application/a2a+json")],
        Some(send_message_payload(
            "task-matrix-tenant-stream",
            "thread-matrix-tenant-stream",
            "msg-matrix-tenant-stream",
            "stream tenant",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("task-matrix-tenant-stream"),
        "missing tenant stream task body: {body}"
    );
}

#[tokio::test]
async fn a2a_http_dispatch_matrix_rejects_version_path_and_content_type_errors() {
    let app = make_test_app(&["alpha"]);

    let (status, body) =
        request_json(&app, "GET", "/v1/a2a/tasks?A2A-Version=0.9", &[], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"]["details"][0]["reason"].as_str(),
        Some("VERSION_NOT_SUPPORTED")
    );

    let (status, body) = request_json(&app, "GET", "/v1/a2a/not/a/route", &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["status"].as_str(), Some("NOT_FOUND"));

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "text/plain")],
        Some(send_message_payload(
            "task-bad-content-type",
            "thread-bad-content-type",
            "msg-bad-content-type",
            "hello",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
    assert_eq!(
        body["error"]["details"][0]["fieldViolations"][0]["field"].as_str(),
        Some("contentType")
    );

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/tasks/task-unsupported:pause",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["status"].as_str(), Some("NOT_FOUND"));

    let (status, body) = request_json(&app, "POST", "/v1/a2a/tasks/:cancel", &[], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
    assert_eq!(
        body["error"]["details"][0]["fieldViolations"][0]["field"].as_str(),
        Some("taskId")
    );

    for uri in [
        "/v1/a2a/tasks/task-missing-config-id/pushNotificationConfigs",
        "/v1/a2a/alpha/tasks/task-missing-config-id/pushNotificationConfigs",
    ] {
        let (status, _content_type, body) = request_text(&app, "DELETE", uri, &[], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "unexpected {uri}: {body}");
    }
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
    assert_eq!(body["capabilities"]["streaming"].as_bool(), Some(true));
    assert_eq!(
        body["capabilities"]["pushNotifications"].as_bool(),
        Some(true)
    );
    assert_eq!(
        body["capabilities"]["extendedAgentCard"].as_bool(),
        Some(false)
    );
    assert!(
        body.get("url").is_none(),
        "top-level url must not be present"
    );
}

#[tokio::test]
async fn message_send_returns_task_wrapper_and_task_is_retrievable() {
    let app = make_test_app(&["alpha"]);
    let task_id = "task-latest-a2a";
    let context_id = "thread-latest-a2a";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(task_id, context_id, "msg-1", "hello")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));
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
async fn a2a_message_send_self_cancel_cascades_background_run_children() {
    let (app, store, manager) = build_background_cancel_fixture();
    let task_id = "task-a2a-self-cancel";
    let context_id = "thread-a2a-self-cancel";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            context_id,
            "msg-a2a-self-cancel",
            "spawn a child and then cancel yourself",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));
    assert_eq!(
        body["task"]["status"]["state"].as_str(),
        Some("TASK_STATE_CANCELED"),
        "expected cancelled A2A task body: {body}"
    );

    let run = store
        .load_run(task_id)
        .await
        .expect("load run")
        .expect("run should exist");
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));

    tokio::time::sleep(Duration::from_millis(50)).await;
    let tasks = manager.list(context_id).await;
    assert_eq!(tasks.len(), 1, "expected one spawned child task: {tasks:?}");
    assert_eq!(tasks[0].status, TaskStatus::Cancelled);
    assert_eq!(tasks[0].task_type, "background_child");
    assert_eq!(
        tasks[0].parent_context.run_id.as_deref(),
        Some(task_id),
        "background child should be linked to the A2A run for cascade cancel"
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
    assert_eq!(
        task["status"]["state"].as_str(),
        Some("TASK_STATE_CANCELED")
    );
}

#[tokio::test]
async fn tenant_message_send_is_visible_in_tenant_task_list() {
    let app = make_test_app(&["alpha"]);
    let task_id = "tenant-task-1";
    let context_id = "tenant-thread-1";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            context_id,
            "msg-tenant-1",
            "hello tenant",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));

    let (status, body) = request_json(&app, "GET", "/v1/a2a/alpha/tasks", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tasks"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tasks"][0]["id"].as_str(), Some(task_id));
    assert_eq!(body["tasks"][0]["contextId"].as_str(), Some(context_id));
}

#[tokio::test]
async fn shared_context_keeps_task_ids_distinct_and_histories_scoped() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );
    let context_id = "thread-shared-context";

    let (status, first) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-shared-1",
            context_id,
            "msg-shared-1",
            "first",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {first}");

    let (status, second) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-shared-2",
            context_id,
            "msg-shared-2",
            "second",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {second}");

    let thread = store
        .load_thread(context_id)
        .await
        .expect("load thread")
        .expect("thread");
    let bindings = thread
        .metadata
        .custom
        .get("a2a.taskBindings")
        .expect("task bindings metadata");
    assert_eq!(
        bindings["tasks"]["task-shared-1"]["start_message_id"].as_str(),
        Some("msg-shared-1")
    );
    assert_eq!(
        bindings["tasks"]["task-shared-1"]["end_message_id"].as_str(),
        Some("msg-shared-2")
    );
    assert!(
        bindings["tasks"]["task-shared-1"]
            .get("history_start")
            .is_none(),
        "task binding should use message id cursor only"
    );
    assert!(
        bindings["tasks"]["task-shared-1"]
            .get("history_end")
            .is_none(),
        "task binding should use message id cursor only"
    );

    let (status, first_task) = request_json(
        &app,
        "GET",
        "/v1/a2a/tasks/task-shared-1?historyLength=10",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_task["contextId"].as_str(), Some(context_id));
    assert_eq!(user_message_ids(&first_task), vec!["msg-shared-1"]);

    let (status, second_task) = request_json(
        &app,
        "GET",
        "/v1/a2a/tasks/task-shared-2?historyLength=10",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_task["contextId"].as_str(), Some(context_id));
    assert_eq!(user_message_ids(&second_task), vec!["msg-shared-2"]);

    let (status, tasks) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks?contextId={context_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task_ids = tasks["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .filter_map(|task| task["id"].as_str())
        .collect::<Vec<_>>();
    assert!(task_ids.contains(&"task-shared-1"));
    assert!(task_ids.contains(&"task-shared-2"));
}

#[tokio::test]
async fn waiting_task_id_resumes_the_same_task() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );
    let task_id = "task-resume";
    let context_id = "thread-resume";
    let existing_messages = vec![Message::user("first turn").with_id("msg-initial".to_string())];
    let waiting_run = RunRecord {
        run_id: task_id.to_string(),
        thread_id: context_id.to_string(),
        agent_id: "alpha".to_string(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Waiting,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        }),
        outcome: None,
        created_at: 1,
        started_at: None,
        finished_at: None,
        updated_at: 1,
        steps: 1,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    store
        .save_thread(&Thread::with_id(context_id))
        .await
        .expect("save thread");
    store
        .checkpoint(context_id, &existing_messages, &waiting_run)
        .await
        .expect("seed waiting run");

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            context_id,
            "msg-resume",
            "resumed input",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));

    let resumed_run = store
        .load_run(task_id)
        .await
        .expect("load resumed run")
        .expect("resumed run present");
    assert_eq!(resumed_run.run_id, task_id);
    assert_eq!(resumed_run.thread_id, context_id);
    assert_eq!(resumed_run.status, RunStatus::Done);

    let (status, task) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}?historyLength=10"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user_message_ids(&task), vec!["msg-initial", "msg-resume"]);
}

#[tokio::test]
async fn message_stream_returns_sse_updates() {
    let app = make_test_app(&["alpha"]);
    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/message:stream",
        &[("content-type", "application/a2a+json")],
        Some(send_message_payload(
            "task-stream",
            "thread-stream",
            "msg-stream",
            "hello",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.contains("text/event-stream"),
        "unexpected content type: {content_type}"
    );
    assert!(body.contains("\"task\""), "missing task payload: {body}");
    assert!(
        body.contains("TASK_STATE_COMPLETED") || body.contains("TASK_STATE_WORKING"),
        "missing task state in stream: {body}"
    );
}

#[tokio::test]
async fn subscribe_stream_returns_updates_for_existing_task() {
    let app = build_test_app(
        &["alpha"],
        Arc::new(DelayedExecutor),
        ServerConfig::default(),
    );
    let task_id = "task-subscribe";
    let context_id = "thread-subscribe";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": task_id,
                "contextId": context_id,
                "messageId": "msg-subscribe",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        &format!("/v1/a2a/tasks/{task_id}:subscribe"),
        &[("content-type", "application/json")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("\"task\""),
        "missing initial task event: {body}"
    );
    assert!(
        body.contains("\"statusUpdate\""),
        "missing status update event: {body}"
    );
    assert!(body.contains("TASK_STATE_COMPLETED"));
}

#[tokio::test]
async fn return_immediately_materializes_a2a_threads_with_timestamps() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(DelayedExecutor),
        ServerConfig::default(),
    );
    let context_id = "thread-a2a-lazy";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-a2a-lazy-1",
                "contextId": context_id,
                "messageId": "msg-a2a-lazy-1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let first = store
        .load_thread(context_id)
        .await
        .expect("load first thread")
        .expect("first thread should exist");
    let created_at = first
        .metadata
        .created_at
        .expect("A2A metadata projection should initialize created_at");
    let first_updated_at = first
        .metadata
        .updated_at
        .expect("A2A metadata projection should initialize updated_at");
    assert!(first.metadata.custom.contains_key("a2a.taskBindings"));

    tokio::time::sleep(Duration::from_millis(20)).await;

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-a2a-lazy-2",
                "contextId": context_id,
                "messageId": "msg-a2a-lazy-2",
                "role": "ROLE_USER",
                "parts": [{"text": "hello again"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let second = store
        .load_thread(context_id)
        .await
        .expect("load second thread")
        .expect("second thread should exist");
    assert_eq!(second.metadata.created_at, Some(created_at));
    assert!(
        second.metadata.updated_at.unwrap_or_default() > first_updated_at,
        "metadata-only A2A updates should refresh updated_at"
    );
}

#[tokio::test]
async fn push_notification_configs_roundtrip_and_inline_delivery_work() {
    use axum::{Json, Router, routing::post};
    use tokio::sync::oneshot;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind webhook listener");
    let webhook_addr = listener.local_addr().expect("local addr");
    let webhook_url = format!("http://{webhook_addr}/notify");
    let (tx, rx) = oneshot::channel::<Value>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let webhook = Router::new().route(
        "/notify",
        post({
            let tx = Arc::clone(&tx);
            move |Json(payload): Json<Value>| {
                let tx = Arc::clone(&tx);
                async move {
                    if let Some(sender) = tx.lock().expect("tx mutex").take() {
                        let _ = sender.send(payload.clone());
                    }
                    Json(json!({"ok": true}))
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, webhook).await.expect("serve webhook");
    });

    let app = make_test_app(&["alpha"]);
    let task_id = "task-push";
    let context_id = "thread-push";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": task_id,
                "contextId": context_id,
                "messageId": "msg-push",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            },
            "configuration": {
                "pushNotificationConfig": {
                    "url": webhook_url,
                    "token": "push-token"
                }
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let delivered = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("webhook delivery timed out")
        .expect("webhook payload should be delivered");
    assert!(
        delivered.get("statusUpdate").is_some() || delivered.get("artifactUpdate").is_some(),
        "unexpected webhook payload: {delivered}"
    );

    let (status, list) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config_id = list["configs"][0]["id"]
        .as_str()
        .expect("config id")
        .to_string();

    let (status, cfg) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs/{config_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["taskId"].as_str(), Some(task_id));

    let (status, _content_type, deleted) = request_text(
        &app,
        "DELETE",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs/{config_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(deleted.is_empty());
}

#[tokio::test]
async fn extended_agent_card_requires_bearer_auth_when_configured() {
    let app = build_test_app(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig {
            a2a_extended_card_bearer_token: Some("secret-token".into()),
            ..Default::default()
        },
    );

    let (status, body) = request_json(&app, "GET", "/.well-known/agent-card.json", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["capabilities"]["extendedAgentCard"].as_bool(),
        Some(true)
    );

    let (status, body) = request_json(&app, "GET", "/v1/a2a/extendedAgentCard", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["status"].as_str(), Some("UNAUTHENTICATED"));

    let (status, body) = request_json(
        &app,
        "GET",
        "/v1/a2a/extendedAgentCard",
        &[("authorization", "Bearer secret-token")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str(), Some("alpha"));
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

// `thread_has_prior_context` previously only checked messages, so a thread
// that had a stored A2A task binding (metadata) but no messages yet would
// still fall through to the public agent. Verify the route now refuses.
#[tokio::test]
async fn send_message_rejects_ambiguous_agent_for_thread_with_task_binding_only() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );

    let context_id = "thread-task-binding-only";
    let mut thread = Thread::with_id(context_id);
    thread.metadata.custom.insert(
        "a2a.taskBindings".to_string(),
        json!({"tasks": {"task-1": {"thread_id": context_id, "start_message_id": "msg-0"}}}),
    );
    store.save_thread(&thread).await.unwrap();

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "contextId": context_id,
                "messageId": "msg-ambiguous-binding",
                "role": "ROLE_USER",
                "parts": [{"text": "follow up"}]
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
}

// When a thread already holds prior messages but no run/dispatch from which
// to read agent_id, the previous behaviour silently fell back to the public
// agent — silently switching the conversation onto a different binding.
// Verify the route now refuses and returns an explicit error.
#[tokio::test]
async fn send_message_rejects_ambiguous_agent_for_existing_thread_with_messages() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );

    let context_id = "thread-ambiguous-agent";
    let thread = Thread::with_id(context_id);
    store.save_thread(&thread).await.unwrap();
    store
        .save_messages(context_id, &[Message::user("hello there")])
        .await
        .unwrap();

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "contextId": context_id,
                "messageId": "msg-ambiguous",
                "role": "ROLE_USER",
                "parts": [{"text": "follow up"}]
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
    assert_eq!(
        body["error"]["details"][0]["fieldViolations"][0]["field"].as_str(),
        Some("agent")
    );
}

// ── Multi-agent / multi-model thread switching ────────────────────

/// Executor that prefixes its response with a distinct marker, so the
/// test can prove which provider (and therefore which agent) executed
/// each turn. Capturing the marker in the assistant message text lets
/// us verify both the per-run agent_id binding AND the per-agent model
/// routing.
struct LabelledExecutor {
    name: &'static str,
    label: &'static str,
}

#[async_trait]
impl LlmExecutor for LabelledExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if m.role == awaken_contract::contract::message::Role::User {
                    Some(m.text())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        Ok(StreamResult {
            content: vec![awaken_contract::contract::content::ContentBlock::text(
                format!("{}: {last_user}", self.label),
            )],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }
    fn name(&self) -> &str {
        self.name
    }
}

fn build_multi_agent_app() -> (axum::Router, Arc<InMemoryStore>) {
    let alpha = Arc::new(LabelledExecutor {
        name: "alpha",
        label: "alpha-out",
    });
    let beta = Arc::new(LabelledExecutor {
        name: "beta",
        label: "beta-out",
    });
    let builder = AgentRuntimeBuilder::new()
        .with_model(ModelSpec::new(
            "model-alpha",
            "alpha-provider",
            "alpha-model",
        ))
        .with_model(ModelSpec::new("model-beta", "beta-provider", "beta-model"))
        .with_provider("alpha-provider", alpha)
        .with_provider("beta-provider", beta)
        .with_agent_spec(AgentSpec {
            id: "agent-alpha".into(),
            model_id: "model-alpha".into(),
            system_prompt: "alpha".into(),
            max_rounds: 1,
            ..Default::default()
        })
        .with_agent_spec(AgentSpec {
            id: "agent-beta".into(),
            model_id: "model-beta".into(),
            system_prompt: "beta".into(),
            max_rounds: 1,
            ..Default::default()
        });
    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        builder
            .with_in_memory_thread_run_store(store.clone())
            .build()
            .expect("build runtime"),
    );
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    (build_router(&state), store)
}

/// Drive the same A2A thread through three turns alternating between
/// two agents bound to different model_ids and provider executors.
/// Asserts:
///   1. Each turn lands on the agent named in the URL path, overriding
///      `latest_run` inference.
///   2. The model used per turn matches the per-agent registry binding
///      (visible through the executor-injected marker in the assistant
///      reply text).
///   3. Thread state persists across switches (history accumulates from
///      every agent's contributions, and run records carry the right
///      agent_id per run).
#[tokio::test]
async fn message_send_can_switch_agent_and_model_on_same_thread() {
    use awaken_contract::contract::storage::{RunQuery, RunStore};

    let (app, store) = build_multi_agent_app();
    let context_id = "thread-multi-agent";

    let turns = [
        (
            "task-alpha-1",
            "/v1/a2a/agent-alpha/message:send",
            "msg-alpha-1",
            "ping one",
            "agent-alpha",
            "model-alpha",
            "alpha-out: ping one",
        ),
        (
            "task-beta-1",
            "/v1/a2a/agent-beta/message:send",
            "msg-beta-1",
            "ping two",
            "agent-beta",
            "model-beta",
            "beta-out: ping two",
        ),
        (
            "task-alpha-2",
            "/v1/a2a/agent-alpha/message:send",
            "msg-alpha-2",
            "ping three",
            "agent-alpha",
            "model-alpha",
            "alpha-out: ping three",
        ),
    ];

    for (task_id, uri, message_id, text, expected_agent, expected_model, expected_reply) in turns {
        let (status, body) = request_json(
            &app,
            "POST",
            uri,
            &[("content-type", "application/json")],
            Some(send_message_payload(task_id, context_id, message_id, text)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri} body: {body}");

        let run = store
            .load_run(task_id)
            .await
            .expect("load run")
            .expect("run record exists");
        assert_eq!(run.agent_id, expected_agent, "wrong agent for {task_id}");
        assert_eq!(run.thread_id, context_id);

        // The activation snapshot must survive checkpoint write-back;
        // previously a stale ThreadContext cache miss dropped it on the
        // second-and-later run in the same thread (P2-R8b).
        let activation = run
            .activation
            .as_ref()
            .unwrap_or_else(|| panic!("activation must persist on run {task_id}"));
        assert!(
            activation
                .resolution_scope
                .entries
                .iter()
                .any(|entry| entry.kind == "agent" && entry.id == expected_agent),
            "pinned manifest must include the active agent {expected_agent}"
        );
        // Model binding correctness is also proven by the labelled-
        // executor reply marker asserted below.
        let _ = expected_model;

        // The reply text must come from THIS agent's labelled executor,
        // proving that the path-agent override actually routed model
        // selection, not just the run record's metadata.
        let messages = store
            .load_messages(context_id)
            .await
            .expect("load messages")
            .unwrap_or_default();
        assert!(
            messages.iter().any(|m| {
                m.role == awaken_contract::contract::message::Role::Assistant
                    && m.text() == expected_reply
            }),
            "missing reply '{expected_reply}' for {task_id}; got history: {:?}",
            messages
                .iter()
                .map(|m| (m.role, m.text()))
                .collect::<Vec<_>>()
        );
    }

    // All three runs are durable under the same thread, each tagged
    // with the agent that the path-tenant override selected for that
    // turn (per-loop assertions above verified each individually;
    // re-confirm via list_runs that they all live under the same
    // thread without dropping any).
    let page = store
        .list_runs(&RunQuery {
            thread_id: Some(context_id.into()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list runs");
    let mut agents: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for run in &page.items {
        *agents.entry(run.agent_id.as_str()).or_insert(0) += 1;
    }
    assert_eq!(
        agents.get("agent-alpha").copied().unwrap_or(0),
        2,
        "expected two agent-alpha runs in {:?}",
        page.items.iter().map(|r| &r.agent_id).collect::<Vec<_>>()
    );
    assert_eq!(
        agents.get("agent-beta").copied().unwrap_or(0),
        1,
        "expected one agent-beta run in {:?}",
        page.items.iter().map(|r| &r.agent_id).collect::<Vec<_>>()
    );

    // A follow-up turn without a path agent must inherit the most
    // recently dispatched agent for the thread. The store tracks
    // insertion order as a tie-break (P2-R8a), so even though all three
    // turns above landed in the same wall-clock second the inferred
    // latest run is deterministically `agent-alpha` (turn 3).
    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-inferred",
            context_id,
            "msg-inferred",
            "ping four",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let inferred = store
        .load_run("task-inferred")
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(
        inferred.agent_id, "agent-alpha",
        "no-tenant follow-up must reuse the most recent turn's agent"
    );
}
